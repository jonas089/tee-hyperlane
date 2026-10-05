//! What a chain has to provide for the enclave to attest its Hyperlane outbox.
//!
//! Two things, and everything else is shared:
//!
//! * `verify` - authenticate a head and return its state root. A chain may run a light client
//!   (Ethereum, Celestia) or check its sequencer's signature (Base, Arbitrum, and Eden, whose
//!   headers come through Celestia's light client). The enclave only needs the root.
//! * `merkle_tree` - prove the Hyperlane merkle tree under a root `verify` returned.
//!
//! Inputs are `serde_json::Value` and each chain parses its own, so nothing outside a chain's
//! module knows its shape. `attest.rs` then does the same thing for every chain: verify the
//! head, read the tree at both ends, replay the batch, sign.
//!
//! Two rules every implementation keeps, because both have been broken before:
//!
//! * **Never take "where to look" from the input.** Anchor contracts, storage slots, sequencer
//!   keys, namespaces: constants in the chain's module, so they sit under the enclave's
//!   measurement. A caller who can name them can point the enclave at state they control and
//!   prove it honestly.
//! * **Never take the clock from the input.** A chain that needs wall time reads the enclave's.

use crate::state::IsmState;
use alloy_primitives::B256;
use hyperlane_types::MerkleTree;
use serde_json::Value;

pub trait Origin: Sync {
    /// Authenticate the head `input` describes, starting from what the ISM already trusts.
    ///
    /// Must check the supplied light-client store, if the chain has one, against
    /// `trusted.lc_store_commit`, and return the commitment the ISM should move to.
    fn verify(&self, input: Value, trusted: &IsmState) -> anyhow::Result<Head>;

    /// Prove the Hyperlane merkle tree under `root`, and say which address it was read from.
    fn merkle_tree(&self, proof: Value, root: B256) -> anyhow::Result<Tree>;
}

/// A head the enclave is willing to attest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub root: B256,
    pub height: u64,
    /// When the attested state was produced. Must not go backwards.
    pub timestamp: u64,
    /// The light-client store commitment the ISM moves to.
    pub store_commit: [u8; 32],
    /// The newest chain time the enclave verified: the head's own time, or for Eden the
    /// Celestia block its header was posted in.
    pub attested_at: u64,
}

/// A Hyperlane tree, and the address it was proven at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tree {
    /// As a Hyperlane message addresses it: 32 bytes, EVM addresses left-padded.
    pub address: [u8; 32],
    pub tree: MerkleTree,
}

/// One origin this enclave can attest.
pub struct Chain {
    /// How requests and route configs name it.
    pub name: &'static str,
    /// Its Hyperlane domain, which the ISM state carries so one origin's attestation cannot
    /// be replayed into another's ISM.
    pub domain: u32,
    pub origin: &'static dyn Origin,
}

/// Every origin compiled into this image. Each image compiles only its own chain.
pub fn chains() -> Vec<&'static Chain> {
    let mut all: Vec<&'static Chain> = Vec::new();
    #[cfg(feature = "celestia")]
    all.push(&crate::chains::l1::celestia::CELESTIA);
    #[cfg(feature = "ethereum")]
    all.push(&crate::chains::l1::ethereum::ETHEREUM);
    #[cfg(feature = "arbitrum")]
    all.push(&crate::chains::l2::arbitrum::ARBITRUM);
    #[cfg(feature = "base")]
    all.push(&crate::chains::l2::base::BASE);
    #[cfg(feature = "eden")]
    all.push(&crate::chains::l2::eden::EDEN);
    all
}

pub fn find(name: &str) -> Option<&'static Chain> {
    chains().into_iter().find(|c| c.name == name)
}

/// The enclave's own clock, in seconds. The one source of wall time any chain may use.
pub fn now() -> anyhow::Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}

/// Parse a chain's input, naming the chain when it does not fit.
pub fn parse<T: serde::de::DeserializeOwned>(what: &str, value: Value) -> anyhow::Result<T> {
    serde_json::from_value(value).map_err(|e| anyhow::anyhow!("malformed {what}: {e}"))
}

/// A verified state root and the head it came from. What each chain's own verification
/// returns before it is turned into a `Head`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttestedRoot {
    pub state_root: B256,
    pub height: u64,
    /// Head timestamp in seconds.
    pub timestamp: u64,
}
