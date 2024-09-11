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

// TODO: Document how BytePoly works.

use std::cmp::max;

use hex::FromHex;
use num_bigint::{BigInt, BigUint};
use num_integer::Integer;
use num_traits::identities::Zero;
use risc0_circuit_recursion::CHECKED_COEFFS_PER_POLY;
use risc0_core::field::{Elem, Field};
use risc0_zkp::{core::digest::Digest, core::hash::HashFn};
use tracing::trace;

pub const BITS_PER_COEFF: usize = 8;

pub fn to_biguint(bp: impl AsRef<[i32]>) -> BigUint {
    let bp = bp.as_ref();
    let mut out = BigInt::default();
    let mut mul = BigInt::from(1usize);
    let coeff_mul = BigInt::from(1usize << BITS_PER_COEFF);
    let mut log = String::new();
    for i in 0..bp.len() {
        if i != 0 {
            log += ", ";
        }
        out += &mul * bp[i];
        mul *= &coeff_mul;
        log += &bp[i].to_string();
    }
    out.to_biguint().expect(&format!("Unable to make unsigned bigint: {log}", log=log))
}

pub fn dump(bp: impl AsRef<[i32]>) -> String {
    let bp = bp.as_ref();
    // tracing::error!("dump will be...");
    // tracing::error!("dump: {}", to_biguint(bp));
    format!("{} ({:?})", to_biguint(bp), bp)
}

pub fn from_biguint(mut val: BigUint, coeffs: usize) -> Vec<i32> {
    let mut out = vec![0; coeffs];
    let mut i = 0;
    let mul = BigUint::from(1usize << BITS_PER_COEFF);
    while !val.is_zero() {
        assert!(i < coeffs, "{val} exceeds {coeffs}");
        let remain;
        (val, remain) = val.div_rem(&mul);
        out[i] = remain
            .try_into()
            .expect("Unable to convert coefficient from bigint");
        i += 1;
    }
    out
}

pub fn from_biguint_fixed<const N: usize>(mut val: BigUint) -> [i32; N] {
    let mut out = [0i32; N];
    let mut i = 0;
    let mul = BigUint::from(1usize << BITS_PER_COEFF);
    while !val.is_zero() {
        assert!(i < N, "{val} exceeds {N}");
        let remain;
        (val, remain) = val.div_rem(&mul);
        out[i] = remain
            .try_into()
            .expect("Unable to convert coefficient from bigint");
        i += 1;
    }
    out
}

pub fn from_hex(hex: &str) -> Vec<i32> {
    let bytes: Vec<u8> = FromHex::from_hex(hex).unwrap();
    bytes.into_iter().map(i32::from).collect()
}

pub fn nondet_quot_fixed<const N: usize>(
    lhs: impl AsRef<[i32]>,
    rhs: impl AsRef<[i32]>,
) -> [i32; N] {
    // tracing::error!("nondet_quot_fixed lhs will be...");
    // tracing::error!("nondet_quot_fixed lhs: {}", to_biguint(lhs));
    let lhs = to_biguint(lhs);
    // tracing::error!("nondet_quot_fixed rhs will be...");
    // tracing::error!("nondet_quot_fixed rhs: {}", to_biguint(rhs));
    let rhs = to_biguint(rhs);
    let quot = lhs.div_floor(&rhs);
    trace!("quot({lhs},{rhs}) = {quot}");
    from_biguint_fixed(quot)
}

pub fn nondet_rem_fixed<const N: usize>(
    lhs: impl AsRef<[i32]>,
    rhs: impl AsRef<[i32]>,
) -> [i32; N] {
    // tracing::error!("nondet_rem_fixed lhs will be...");
    // tracing::error!("nondet_rem_fixed lhs: {}", to_biguint(lhs));
    // tracing::error!("nondet_rem_fixed rhs will be...");
    // tracing::error!("nondet_rem_fixed rhs: {}", to_biguint(rhs));
    let rem = to_biguint(lhs).mod_floor(&to_biguint(rhs));
    from_biguint_fixed(rem)
}

pub fn nondet_inv_fixed<const N: usize>(
    lhs: impl AsRef<[i32]>,
    rhs: impl AsRef<[i32]>,
) -> [i32; N] {
    // Computes the inverse of LHS mod RHS via the `inv = lhs^{rhs - 2} % rhs` algorithm
    // Note that this assumes `rhs` is prime. For non-prime `rhs`, this algorithm can
    // fail (compute an incorrect inverse). Note that this is not a soundness problem, as
    // this is a nondet and the correctness of the inversion must be checked inside the
    // circuit regardless.
    // tracing::error!("nondet_inv_fixed lhs will be...");
    // tracing::error!("nondet_inv_fixed lhs: {}", to_biguint(lhs));
    // tracing::error!("nondet_inv_fixed rhs will be...");
    // tracing::error!("nondet_inv_fixed rhs: {}", to_biguint(rhs));
    let lhs = to_biguint(lhs);
    let rhs = to_biguint(rhs);
    let exp = rhs.clone() - 2u8;
    let result = lhs.modpow(&exp, &rhs);
    trace!("inv({lhs}, [mod] {rhs}) = {result}");
    from_biguint_fixed(result)
}

// Returns variable length BytePolys to be added to the private witness.
pub fn eval_constraint(
    val: impl AsRef<[i32]>,
    carry_offset: usize,
    carry_bytes: usize,
) -> Vec<Vec<i32>> {
    let val = val.as_ref();
    let mut carry_polys: Vec<Vec<i32>> = Vec::new();
    carry_polys.resize(carry_bytes, vec![0; val.len()]);

    let mut carry: i32 = 0;
    for i in 0..val.len() {
        carry = (val[i] + carry) / 256;
        let carry_u = carry + carry_offset as i32;
        carry_polys[0][i] = carry_u & 0xFF;
        if carry_bytes > 1 {
            carry_polys[1][i] = (carry_u >> 8) & 0xff;
        }
        if carry_bytes > 2 {
            carry_polys[2][i] = (carry_u >> 16) & 0xff;
            carry_polys[3][i] = ((carry_u >> 16) & 0xff) * 4;
        }
    }

    // Verify carry computation
    let mut big_carry = vec![0; val.len()];
    for i in 0..val.len() {
        big_carry[i] = carry_polys[0][i];
        if carry_bytes > 1 {
            big_carry[i] += 256 * carry_polys[1][i];
        }
        if carry_bytes > 2 {
            big_carry[i] += 65536 * carry_polys[2][i];
        }
        big_carry[i] -= carry_offset as i32;
    }

    for i in 0..val.len() {
        let mut should_be_zero: i32 = val[i];
        should_be_zero -= 256 * big_carry[i];
        if i != 0 {
            should_be_zero += big_carry[i - 1];
        }
        assert_eq!(should_be_zero, 0, "Invalid carry computation");
    }

    carry_polys
}

/// Packs this byte poly into u32s, 4 bytes per u32, for use in
/// calculating digests.  Each byte must be normalized, i.e. fit
/// into a u8.
pub fn into_padded_u32s(bp: impl AsRef<[i32]>) -> Vec<u32> {
    let bp = bp.as_ref();
    const WORD_SIZE: usize = std::mem::size_of::<u32>();
    let padded_coeffs = bp.len().div_ceil(CHECKED_COEFFS_PER_POLY) * CHECKED_COEFFS_PER_POLY;
    let mut out = Vec::from(bp);
    out.resize(padded_coeffs, 0i32);

    out.chunks(WORD_SIZE)
        // Convert from [i32] to [i32; 4]
        .map(|chunk| {
            <[i32; 4]>::try_from(chunk)
                .expect("Encountered unexpected partial chunk; is padding logic wrong?")
        })
        // Convert from [i32; 4] to [u8; 4]
        .map(|chunk| {
            chunk.map(|coeff| {
                u8::try_from(coeff)
                    .expect("Coefficient out of range; byte poly should be normalized")
            })
        })
        // Convert from [u8; 4] to u32
        .map(u32::from_le_bytes)
        .collect()
}

pub fn add_fixed<const N: usize>(lhs: impl AsRef<[i32]>, rhs: impl AsRef<[i32]>) -> [i32; N] {
    let lhs = lhs.as_ref();
    let rhs = rhs.as_ref();
    assert_eq!(N, max(lhs.len(), rhs.len()));
    core::array::from_fn(|i| lhs.get(i).unwrap_or(&0) + rhs.get(i).unwrap_or(&0))
}

pub fn sub_fixed<const N: usize>(lhs: impl AsRef<[i32]>, rhs: impl AsRef<[i32]>) -> [i32; N] {
    let lhs = lhs.as_ref();
    let rhs = rhs.as_ref();
    assert_eq!(N, max(lhs.len(), rhs.len()));
    core::array::from_fn(|i| lhs.get(i).unwrap_or(&0) - rhs.get(i).unwrap_or(&0))
}

pub fn mul_fixed<const N: usize>(lhs: impl AsRef<[i32]>, rhs: impl AsRef<[i32]>) -> [i32; N] {
    let lhs = lhs.as_ref();
    let rhs = rhs.as_ref();
    assert_eq!(N, lhs.len() + rhs.len());
    let mut out = [0; N];
    for (i, a) in lhs.iter().enumerate() {
        for (j, b) in rhs.iter().enumerate() {
            out[i + j] += a * b;
        }
    }
    out
}

pub fn compute_digest<F: Field>(
    hash: &dyn HashFn<F>,
    witness: &[impl AsRef<[i32]>],
    group_count: usize,
) -> Digest {
    let mut group: usize = 0;
    let mut cur = [F::Elem::ZERO; CHECKED_COEFFS_PER_POLY];
    let mut elems = Vec::new();

    for wit in witness.iter() {
        for chunk in wit.as_ref().chunks(CHECKED_COEFFS_PER_POLY) {
            for (k, elem) in cur.iter_mut().enumerate() {
                *elem = *elem * F::Elem::from_u64(1u64 << BITS_PER_COEFF)
                    + F::Elem::from_u64(*chunk.get(k).unwrap_or(&0) as u64);
            }
            group += 1;
            if group == group_count {
                elems.extend(cur);
                cur = Default::default();
                group = 0;
            }
        }
    }
    if group != 0 {
        elems.extend(cur);
    }
    *hash.hash_elem_slice(&elems)
}
