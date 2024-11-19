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

use alloc::collections::VecDeque;
use anyhow::Result;
use risc0_binfmt::read_sha_halfs;
use risc0_circuit_keccak::prove::{prove, verify};
use risc0_core::field::baby_bear::BabyBearElem;
use risc0_zkp::{
    core::{
        digest::{Digest, DIGEST_SHORTS},
        hash::poseidon2::Poseidon2HashSuite,
    },
    hal::cpu::CpuHal,
};

use crate::{receipt::SuccinctReceipt, recursion, Unknown};

use super::client::env::ProveKeccakRequest;

/// Generate a keccak proof that has been lifted.
pub fn prove_keccak(po2: u64, input: &[u8], claim: &Digest) -> Result<SuccinctReceipt<Unknown>> {
    let req = ProveKeccakRequest {
        po2,
        input: input.to_vec(),
        claim_digest: *claim,
    };
    let hash_suite = Poseidon2HashSuite::new_suite();
    let hal = CpuHal::new(hash_suite.clone());
    let input_u32s: &[u32] = bytemuck::cast_slice(req.input.as_slice());
    let input: VecDeque<u32> = Vec::from(input_u32s).into();
    let circuit_hal = risc0_circuit_keccak::prove::cpu::CpuCircuitHal::new(input);
    let control_root: Digest = *risc0_circuit_keccak::get_control_root(req.po2 as usize);
    let seal = prove(&hal, &circuit_hal, req.po2 as usize).unwrap();
    let claim_digest: Digest = read_sha_halfs(&mut VecDeque::from_iter(
        bytemuck::checked::cast_slice::<_, BabyBearElem>(&seal[0..DIGEST_SHORTS])
            .iter()
            .copied()
            .map(u32::from),
    ))?;

    // Make sure we have a valid seal so we can fail early if anything went wrong
    verify(seal.as_slice(), &hash_suite).expect("Verification failed");

    let claim_sha_input = claim_digest
        .as_words()
        .iter()
        .copied()
        .flat_map(|x| [x & 0xffff, x >> 16])
        .map(BabyBearElem::new)
        .collect::<Vec<_>>();

    let mut zkr_input: Vec<u32> = Vec::new();
    zkr_input.extend(control_root.as_words());
    zkr_input.extend(seal);
    zkr_input.extend(bytemuck::cast_slice(claim_sha_input.as_slice()));

    recursion::prove::prove_zkr(
        risc0_circuit_keccak::get_control_id(req.po2 as usize),
        bytemuck::cast_slice(zkr_input.as_slice()).into(),
    )
}
