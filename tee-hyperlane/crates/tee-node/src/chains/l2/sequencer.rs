//! Checks a secp256k1 signature from a rollup's sequencer, for Base and Arbitrum.
//!
//! The key is a constant in each chain's file, so it sits under that image's measurement.

use alloy_primitives::{Address, Signature, B256};

/// Check that `signature` (65 bytes, `r || s || v`) over `hash` was made by `sequencer`.
pub fn verify(hash: B256, signature: &[u8], sequencer: Address) -> anyhow::Result<()> {
    let signature = Signature::from_raw(signature)
        .map_err(|e| anyhow::anyhow!("malformed sequencer signature: {e}"))?;
    let signer = signature
        .recover_address_from_prehash(&hash)
        .map_err(|e| anyhow::anyhow!("sequencer signature does not recover: {e}"))?;
    anyhow::ensure!(
        signer == sequencer,
        "signed by {signer}, not the pinned sequencer {sequencer}"
    );
    Ok(())
}
