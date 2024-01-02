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

use anyhow::{bail, Result};

use crate::{
    host::receipt::{InnerReceipt, SegmentReceipt, SuccinctReceipt},
    ProverServer, Receipt, Segment, Session, VerifierContext,
};

/// An implementation of a [ProverServer] for development and testing purposes.
///
/// This DevModeProver does not produce an actual proof.
/// Instead, the guest code is executed and a fake receipt is returned with
/// accurate journal contents but no cryptographic information.
/// Because the receipt is fake, a verifier can only "verify" this receipt
/// if dev mode is turned on; verification will otherwise fail.
///
/// CONVENIENT, BUT NOT MEANT FOR PRODUCTION
/// Dev mode supports rapid development by allowing the developer to quickly
/// iterate on code without being forced to wait for proving to complete.
/// However, it must not be used in production as it provides no security
/// whatsoever.
///
/// How to enable and disable dev mode:
/// Dev mode is only used when the environment variable `RISC0_DEV_MODE` is set.
/// It can be fully disabled at compile time, regardless of environment
/// variables, by setting the feature flag `disable-dev-mode` on the
/// `risc0_zkvm` crate.
pub struct DevModeProver;

impl ProverServer for DevModeProver {
    fn prove_session(&self, _ctx: &VerifierContext, session: &Session) -> Result<Receipt> {
        eprintln!(
            "WARNING: Proving in dev mode does not generate a valid receipt. \
            Receipts generated from this process are invalid and should never be used in production."
        );

        if cfg!(feature = "disable-dev-mode") {
            bail!(
                "zkVM: dev mode is disabled. Unset RISC0_DEV_MODE environment variable to produce valid proofs"
            )
        }

        let claim = session.get_claim()?;
        Ok(Receipt::new(
            InnerReceipt::Fake { claim },
            session.journal.clone().unwrap_or_default().bytes,
        ))
    }

    fn prove_segment(&self, _ctx: &VerifierContext, _segment: &Segment) -> Result<SegmentReceipt> {
        unimplemented!("This is unsupported for dev mode.")
    }

    fn get_peak_memory_usage(&self) -> usize {
        0
    }

    fn lift(&self, _receipt: &SegmentReceipt) -> Result<SuccinctReceipt> {
        unimplemented!("This is unsupported for dev mode.")
    }

    fn join(&self, _a: &SuccinctReceipt, _b: &SuccinctReceipt) -> Result<SuccinctReceipt> {
        unimplemented!("This is unsupported for dev mode.")
    }

    fn resolve(
        &self,
        _conditional: &SuccinctReceipt,
        _corroborating: &SuccinctReceipt,
    ) -> Result<SuccinctReceipt> {
        unimplemented!("This is unsupported for dev mode.")
    }

    fn identity_p254(&self, _a: &SuccinctReceipt) -> Result<SuccinctReceipt> {
        unimplemented!("This is unsupported for dev mode.")
    }
}
