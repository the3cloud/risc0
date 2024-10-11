// Copyright 2024 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Functions for interacting with the host environment.
//!
//! The zkVM provides a set of functions to perform operations that manage
//! execution, I/O, and proof composition. The set of functions related to each
//! of these operations are described below.
//!
//! ## System State
//!
//! The guest has some control over the execution of the zkVM by pausing or
//! exiting the program explicitly. This can be achieved using the [pause] and
//! [exit] functions.
//!
//! ## Proof Verification
//!
//! The zkVM supports verification of RISC Zero [receipts] in a guest program,
//! enabling [proof composition]. This can be achieved using the [verify()] and
//! [verify_integrity] functions.
//!
//! ## Input and Output
//!
//! The zkVM provides a set of functions for handling input, public output, and
//! private output. This is useful when interacting with the host and committing
//! to some data publicly.
//!
//! The zkVM provides functions that automatically perform (de)serialization on
//! types and, for performance reasons, there is also a `_slice` variant that
//! works with raw slices of plain old data. Performing operations on slices is
//! more efficient, saving cycles during execution and consequently producing
//! smaller proofs that are faster to produce. However, the `_slice` variants
//! can be less ergonomic, so consider trade-offs when choosing between the two.
//! For more information about guest optimization, see RISC Zero's [instruction
//! on guest optimization][guest-optimization]
//!
//! Convenience functions to read and write to default file descriptors are
//! provided. See [read()], [write()], [self::commit] (and their `_slice`
//! variants) for more information.
//!
//! In order to access default file descriptors directly, see [stdin], [stdout],
//! [stderr] and [journal]. These file descriptors are either [FdReader] or
//! [FdWriter] instances, which can be used to read from or write to the host.
//! To read from or write into them, use the [Read] and [Write] traits.
//!
//! WARNING: Specifying a file descriptor with the same value of a default file
//! descriptor is not recommended and may lead to unexpected behavior. A list of
//! default file descriptors can be found in the [fileno] module.
//!
//! ## Utility
//!
//! The zkVM provides utility functions to log messages to the debug console and
//! to measure the number of processor cycles that have occurred since the guest
//! began. These can be achieved using the [log] and [cycle_count] functions.
//!
//! [receipts]: crate::Receipt
//! [proof composition]:https://www.risczero.com/blog/proof-composition
//! [guest-optimization]:
//!     https://dev.risczero.com/api/zkvm/optimization#when-reading-data-as-raw-bytes-use-envread_slice

mod read;
mod verify;
mod write;

use alloc::{
    alloc::{alloc, Layout},
    vec,
};

use anyhow::{bail, Result};
use bytemuck::Pod;
use core::cell::OnceCell;
use risc0_zkvm_platform::{
    align_up, fileno,
    syscall::{
        self, sys_cycle_count, sys_exit, sys_fork, sys_halt, sys_input, sys_log, sys_pause,
        syscall_2, SyscallName,
    },
    WORD_SIZE,
};
use serde::{de::DeserializeOwned, Serialize};

use crate::{
    sha::{
        self,
        rust_crypto::{Digest as _, Sha256},
        Digest, Digestible,
    },
    Assumptions, MaybePruned, Output,
};

pub use self::{
    read::{FdReader, Read},
    verify::{verify, verify_assumption, verify_integrity, VerifyIntegrityError},
    write::{FdWriter, Write},
};

static mut HASHER: OnceCell<Sha256> = OnceCell::new();

/// Digest of the running list of [Assumptions], generated by the [self::verify] and
/// [self::verify_integrity] calls made by the guest.
static mut ASSUMPTIONS_DIGEST: MaybePruned<Assumptions> = MaybePruned::Pruned(Digest::ZERO);

/// A random 16 byte value initialized to random data, provided by the host, on
/// guest start and upon resuming from a pause. Setting this value ensures that
/// the total memory image has at least 128 bits of entropy, preventing
/// information leakage through the post-state digest.
static mut MEMORY_IMAGE_ENTROPY: [u32; 4] = [0u32; 4];

/// Keccak is proven in batches.
pub struct KeccakBatcher {
    input_transcript: [u8; Self::KECCAK_LIMIT],
    block_count_offset: usize,
    data_offset: usize,
}

const fn batcher() -> KeccakBatcher {
    KeccakBatcher {
        input_transcript: [0u8; KeccakBatcher::KECCAK_LIMIT],
        block_count_offset: 0,
        data_offset: KeccakBatcher::BLOCK_COUNT_BYTES,
    }
}

impl Default for KeccakBatcher {
    /// create a new instance of a batcher with an input transcript region
    fn default() -> Self {
        Self {
            input_transcript: [0u8; Self::KECCAK_LIMIT],
            block_count_offset: 0,
            data_offset: Self::BLOCK_COUNT_BYTES,
        }
    }
}

impl KeccakBatcher {
    const KECCAK_LIMIT: usize = 10000;
    const BLOCK_COUNT_BYTES: usize = 8;
    const BLOCK_BYTES: usize = 136;

    /// write data to the input transcript.
    ///
    /// This is meant to be used by lower-level functions within keccak crates.
    /// Many keccak crates will write raw data and padding to a 1600 bit buffer
    /// often called the "state". All data and padding written to the state
    /// should be passed to this function.
    pub fn write_data(&mut self, input: &[u8]) -> Result<()> {
        if self.data_offset + input.len() > Self::KECCAK_LIMIT {
            bail!("keccak input limit exceeded")
        }

        self.input_transcript[self.data_offset..self.data_offset + input.len()]
            .copy_from_slice(input);
        self.data_offset += input.len();

        Ok(())
    }

    /// write padding to the input transcript.
    ///
    /// Pad the raw input with the delimitor, 0x00 bytes, and a 0x80 byte. This
    /// will pad the raw data upto the current block boundary.
    pub fn write_padding(&mut self) -> Result<()> {
        self.write_data(&[0x01])?;
        let data_length = self.current_data_length();
        let remaining_bytes = Self::BLOCK_BYTES - (data_length % Self::BLOCK_BYTES);
        if self.data_offset + remaining_bytes > Self::KECCAK_LIMIT {
            bail!("keccak input limit exceeded")
        }
        let zeroes = vec![0u8; remaining_bytes - 1];

        self.write_data(&zeroes)?;
        self.write_data(&[0x80])?;
        if self.current_data_length() % Self::BLOCK_BYTES != 0 {
            bail!(
                "keccak data was not padded properly. Expected a multiple of {} bytes, got {data_length} bytes", Self::BLOCK_BYTES
            );
        }

        Ok(())
    }

    /// write keccak hash to the transcript, updating the block count.
    ///
    /// the amount of raw data written to the
    pub fn write_hash(&mut self, input: &[u8]) -> Result<()> {
        let data_length = self.current_data_length();
        // at this point, it is expected that the data written is a multiple of
        // the block count.
        if data_length % Self::BLOCK_BYTES != 0 {
            bail!(
                "keccak data was not padded properly. Expected a multiple of {} bytes, got {data_length} bytes", Self::BLOCK_BYTES
            );
        }

        let block_count = (data_length / Self::BLOCK_BYTES) as u8; // TODO: error handling...

        //self::log(alloc::format!("block count: {block_count}"));

        self.write_data(input)?;
        self.input_transcript[self.block_count_offset] = block_count;
        self.block_count_offset = self.data_offset; // TODO: write zeros to the block count region
        self.data_offset += Self::BLOCK_COUNT_BYTES;
        Ok(())
    }

    /// get the digest of the input transcript
    pub fn finalize(&mut self) -> Result<Digest> {
        // todo: return correct slice with size
        if self.data_offset + Self::BLOCK_COUNT_BYTES > Self::KECCAK_LIMIT {
            bail!("keccak input limit exceeded")
        }

        self.input_transcript
            [self.block_count_offset..self.block_count_offset + Self::BLOCK_COUNT_BYTES]
            .copy_from_slice(&[0u8; Self::BLOCK_COUNT_BYTES]);
        Ok(
            *<sha::Impl as risc0_zkp::core::hash::sha::Sha256>::hash_bytes(
                &self.input_transcript[0..self.block_count_offset + Self::BLOCK_COUNT_BYTES],
            ),
        )
    }

    fn current_data_length(&self) -> usize {
        self.data_offset - (self.block_count_offset + Self::BLOCK_COUNT_BYTES)
    }

    /// testing only: get the transcript
    #[cfg(test)]
    pub fn transcript(&self) -> &[u8] {
        &self.input_transcript[0..self.block_count_offset + Self::BLOCK_COUNT_BYTES]
    }
}

/// TODO
pub static mut KECCAK_BATCHER: KeccakBatcher = batcher();

/// Initialize globals before program main
pub(crate) fn init() {
    unsafe {
        #[allow(static_mut_refs)]
        HASHER.set(Sha256::new()).unwrap();
        #[allow(static_mut_refs)]
        syscall::sys_rand(
            MEMORY_IMAGE_ENTROPY.as_mut_ptr(),
            MEMORY_IMAGE_ENTROPY.len(),
        )
    }
}

/// Finalize execution
pub(crate) fn finalize(halt: bool, user_exit: u8) {
    unsafe {
        #[allow(static_mut_refs)]
        let hasher = HASHER.take();
        let journal_digest: Digest = hasher.unwrap().finalize().as_slice().try_into().unwrap();
        #[allow(static_mut_refs)]
        let output = Output {
            journal: MaybePruned::Pruned(journal_digest),
            assumptions: MaybePruned::Pruned(ASSUMPTIONS_DIGEST.digest()),
        };
        let output_words: [u32; 8] = output.digest().into();

        if halt {
            sys_halt(user_exit, &output_words)
        } else {
            sys_pause(user_exit, &output_words)
        }
    }
}

/// Terminate execution of the zkVM.
///
/// Use an exit code of 0 to indicate success, and non-zero to indicate an error.
pub fn exit(exit_code: u8) -> ! {
    finalize(true, exit_code);
    unreachable!();
}

/// Pause the execution of the zkVM.
///
/// Execution may be continued at a later time.
/// Use an exit code of 0 to indicate success, and non-zero to indicate an error.
pub fn pause(exit_code: u8) {
    finalize(false, exit_code);
    init();
}

/// Exchange data with the host.
pub fn syscall(syscall: SyscallName, to_host: &[u8], from_host: &mut [u32]) -> syscall::Return {
    unsafe {
        syscall_2(
            syscall,
            from_host.as_mut_ptr(),
            from_host.len(),
            to_host.as_ptr() as u32,
            to_host.len() as u32,
        )
    }
}

/// Exchanges slices of plain old data with the host.
///
/// This makes two calls to the given syscall; the first gets the length of the
/// buffer to allocate for the return data, and the second actually
/// receives the return data.
///
/// On the host side, implement SliceIo to provide a handler for this call.
///
/// NOTE: This method never frees up the buffer memory storing the host's response.
pub fn send_recv_slice<T: Pod, U: Pod>(syscall_name: SyscallName, to_host: &[T]) -> &'static [U] {
    let syscall::Return(nbytes, _) = syscall(syscall_name, bytemuck::cast_slice(to_host), &mut []);
    let nwords = align_up(nbytes as usize, WORD_SIZE) / WORD_SIZE;
    let from_host_buf = unsafe {
        let layout = Layout::from_size_align(nwords * WORD_SIZE, WORD_SIZE).unwrap();
        core::slice::from_raw_parts_mut(alloc(layout) as *mut u32, nwords)
    };
    syscall(syscall_name, &[], from_host_buf);
    &bytemuck::cast_slice(from_host_buf)[..nbytes as usize / core::mem::size_of::<U>()]
}

/// Read private data from the STDIN of the zkVM and deserializes it.
///
/// This function operates on every [`DeserializeOwned`] type, so you can
/// specify complex types as data to be read and it'll be deserialized
/// automatically.
///
/// # Example
///
/// ```no_run
/// use risc0_zkvm::guest::env;
/// use std::collections::BTreeMap;
///
/// let input: Option<BTreeMap<u64, bool>> = env::read();
/// ```
///
/// More examples can be found in RISC Zero's [example page].
///
/// Additional explanation on I/O in the zkVM can be found in RISC Zero's [I/O documentation].
///
/// [example page]: https://dev.risczero.com/api/zkvm/examples
/// [I/O documentation]: https://dev.risczero.com/api/zkvm/tutorials/io
pub fn read<T: DeserializeOwned>() -> T {
    stdin().read()
}

/// Read a slice from the STDIN of the zkVM.
///
/// This function reads a slice of [plain old data][bytemuck::Pod], not
/// incurring in deserialization overhead. Recommended for performance
/// optimizations. For more context on this, see RISC Zero's [instructions on
/// guest optimization].
///
/// # Example
///
/// ```no_run
/// use risc0_zkvm::guest::env;
///
/// let len: usize = env::read();
/// let mut slice = vec![0u8; len];
/// env::read_slice(&mut slice);
///
/// assert_eq!(slice.len(), len);
/// ```
///
/// More examples can be found in RISC Zero's [example page].
///
/// Additional explanation on I/O in the zkVM can be found in RISC Zero's [I/O documentation].
///
/// [example page]: https://dev.risczero.com/api/zkvm/examples
/// [I/O documentation]: https://dev.risczero.com/api/zkvm/tutorials/io
/// [instructions on guest optimization]: https://dev.risczero.com/api/zkvm/optimization#when-reading-data-as-raw-bytes-use-envread_slice
pub fn read_slice<T: Pod>(slice: &mut [T]) {
    stdin().read_slice(slice)
}

/// Serialize the given data and write it to the STDOUT of the zkVM.
///
/// This is available to the host as the private output on the prover.
/// Some implementations, such as [risc0-r0vm] will also write the data to
/// the host's stdout file descriptor. It is not included in the receipt.
///
/// # Example
///
/// ```no_run
/// use risc0_zkvm::guest::env;
/// use std::collections::BTreeMap;
///
/// let output: BTreeMap<u64, bool> = BTreeMap::from([
///    (1, true),
///    (2, false),
/// ]);
///
/// env::write(&output);
/// ```
///
/// More examples can be found in RISC Zero's [example page].
///
/// Additional explanation on I/O in the zkVM can be found in RISC Zero's [I/O documentation].
///
/// [example page]: https://dev.risczero.com/api/zkvm/examples
/// [I/O documentation]: https://dev.risczero.com/api/zkvm/tutorials/io
pub fn write<T: Serialize>(data: &T) {
    stdout().write(data)
}

/// Write the given slice to the STDOUT of the zkVM.
///
/// This is available to the host as the private output on the prover.
/// Some implementations, such as [risc0-r0vm] will also write the data to
/// the host's stdout file descriptor. It is not included in the receipt.
///
/// This function reads a slice of [plain old data][bytemuck::Pod], not
/// incurring in deserialization overhead. Recommended for performance
/// optimizations. For more context on this, see RISC Zero's [instructions on
/// guest optimization].
///
/// # Example
///
/// ```no_run
/// use risc0_zkvm::guest::env;
///
/// let slice = [1u8, 2, 3, 4];
/// env::write_slice(&slice);
/// ```
///
/// More examples can be found in RISC Zero's [example page].
///
/// Additional explanation on I/O in the zkVM can be found in RISC Zero's [I/O documentation].
///
/// [example page]: https://dev.risczero.com/api/zkvm/examples
/// [I/O documentation]: https://dev.risczero.com/api/zkvm/tutorials/io
/// [instructions on guest optimization]: https://dev.risczero.com/api/zkvm/optimization#when-reading-data-as-raw-bytes-use-envread_slice
pub fn write_slice<T: Pod>(slice: &[T]) {
    stdout().write_slice(slice);
}

/// Serialize the given data and commit it to the journal.
///
/// Data in the journal is included in the receipt and is available to the
/// verifier. It is considered "public" data.
///
/// # Example
///
/// ```no_run
/// use risc0_zkvm::guest::env;
/// use std::collections::BTreeMap;
///
/// let data: BTreeMap<u64, bool> = BTreeMap::from([
///   (1, true),
///   (2, false),
/// ]);
///
/// env::commit(&data);
/// ```
///
/// More examples can be found in RISC Zero's [example page].
///
/// Additional explanation on I/O in the zkVM can be found in RISC Zero's [I/O documentation].
///
/// [example page]: https://dev.risczero.com/api/zkvm/examples
/// [I/O documentation]: https://dev.risczero.com/api/zkvm/tutorials/io
pub fn commit<T: Serialize>(data: &T) {
    journal().write(data)
}

/// Commit the given slice to the journal.
///
/// Data in the journal is included in the receipt and is available to the
/// verifier. It is considered "public" data.
///
/// This function reads a slice of [plain old data][bytemuck::Pod], not
/// incurring in deserialization overhead. Recommended for performance
/// optimizations. For more context on this, see RISC Zero's [instructions on
/// guest optimization].
///
/// # Example
///
/// ```no_run
/// use risc0_zkvm::guest::env;
///
/// let slice = [1u8, 2, 3, 4];
/// env::commit_slice(&slice);
/// ```
///
/// More examples can be found in RISC Zero's [example page].
///
/// Additional explanation on I/O in the zkVM can be found in RISC Zero's [I/O documentation].
///
/// [example page]: https://dev.risczero.com/api/zkvm/examples
/// [I/O documentation]: https://dev.risczero.com/api/zkvm/tutorials/io
/// [instructions on guest optimization]: https://dev.risczero.com/api/zkvm/optimization#when-reading-data-as-raw-bytes-use-envread_slice
pub fn commit_slice<T: Pod>(slice: &[T]) {
    journal().write_slice(slice);
}

/// Return the number of processor cycles that have occurred since the guest
/// began.
///
/// WARNING: The cycle count is provided by the host and is not checked by the zkVM circuit.
pub fn cycle_count() -> u64 {
    sys_cycle_count()
}

/// Print a message to the debug console.
pub fn log(msg: &str) {
    let msg = msg.as_bytes();
    unsafe {
        sys_log(msg.as_ptr(), msg.len());
    }
}

/// Return a writer for STDOUT.
pub fn stdout() -> FdWriter<impl for<'a> Fn(&'a [u8])> {
    FdWriter::new(fileno::STDOUT, |_| {})
}

/// Return a writer for STDERR.
pub fn stderr() -> FdWriter<impl for<'a> Fn(&'a [u8])> {
    FdWriter::new(fileno::STDERR, |_| {})
}

/// Return a writer for the JOURNAL.
pub fn journal() -> FdWriter<impl for<'a> Fn(&'a [u8])> {
    FdWriter::new(fileno::JOURNAL, |bytes| {
        #[allow(static_mut_refs)]
        unsafe {
            HASHER.get_mut().unwrap_unchecked().update(bytes)
        };
    })
}

/// Return a reader for the standard input
pub fn stdin() -> FdReader {
    FdReader::new(fileno::STDIN)
}

/// Read the input digest from the input commitment.
pub fn input_digest() -> Digest {
    Digest::new([
        sys_input(0),
        sys_input(1),
        sys_input(2),
        sys_input(3),
        sys_input(4),
        sys_input(5),
        sys_input(6),
        sys_input(7),
    ])
}

/// Run the given function without proving that it was executed correctly.
///
/// This does not provide any guarantees about the soundness of the execution,
/// but can potentially be executed faster.
#[stability::unstable]
pub fn run_unconstrained(f: impl FnOnce()) {
    let pid = sys_fork();
    if pid == 0 {
        f();
        sys_exit(0)
    }
}

/// Read a frame from the host via `stdin`.
///
/// A frame contains a length header along with the payload. Reading a frame can
/// be more efficient than deserializing a message on-demand. On-demand
/// deserialization can cause many syscalls, whereas a frame will only have two.
#[stability::unstable]
pub fn read_frame() -> alloc::vec::Vec<u8> {
    let mut len: u32 = 0;
    read_slice(core::slice::from_mut(&mut len));
    let mut bytes = vec![0u8; len as usize];
    read_slice(&mut bytes);
    bytes
}

/// Read a frame from the host via `stdin` and deserialize it using the `risc0` codec.
#[stability::unstable]
pub fn read_framed<T: DeserializeOwned>() -> Result<T, crate::serde::Error> {
    crate::serde::from_slice(&read_frame())
}

/// Internal API used for testing. Do not use.
#[stability::unstable]
#[cfg(feature = "std")]
pub fn read_buffered<T: DeserializeOwned>() -> Result<T, crate::serde::Error> {
    let mut len: u32 = 0;
    read_slice(core::slice::from_mut(&mut len));
    let reader = std::io::BufReader::with_capacity(len as usize, stdin());
    T::deserialize(&mut crate::serde::Deserializer::new(reader))
}
