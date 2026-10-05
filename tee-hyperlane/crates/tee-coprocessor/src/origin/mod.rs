//! The `Indexer` trait each origin chain implements: gather proofs, index messages, create a genesis state.

use std::ops::Range;
use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tee_node::state::IsmState;

/// Reading EVM state over JSON-RPC, shared by the EVM origins.
pub mod evm;
/// Chains with their own consensus, verified by a light client.
pub mod l1;
/// Rollups, verified by their sequencer's signature.
pub mod l2;

#[async_trait]
pub trait Indexer: Send + Sync {
    async fn gather(&self, trusted: &IsmState) -> Result<Step>;
    async fn index(&self, from: u64, to: u64) -> Result<Vec<Message>>;
    async fn bootstrap(&self, identity: [u8; 32], height: Option<u64>) -> Result<IsmState>;
}

/// One step of an ISM: the head to move to, and what the enclave needs to verify it.
pub struct Step {
    /// The origin height this step attests.
    pub head: u64,
    /// The tree's size at the trusted height, up to its size at `head`. Empty means idle.
    pub leaves: Range<u32>,
    /// The enclave's chain name, then that chain's input and its two tree proofs.
    pub chain: &'static str,
    pub input: Value,
    pub tree: Value,
    pub tree_snapshot: Value,
    /// The merkle tree hook, as the ISM pins it.
    pub tree_address: [u8; 32],
}

impl Step {
    /// Nothing to attest this tick, and how far the origin could have gone.
    pub fn idle(head: u64) -> Self {
        Self {
            head,
            leaves: 0..0,
            chain: "",
            input: Value::Null,
            tree: Value::Null,
            tree_snapshot: Value::Null,
            tree_address: [0; 32],
        }
    }
}

/// A message as it was inserted into the origin's tree.
#[derive(Debug, Clone)]
pub struct Message {
    pub id: [u8; 32],
    pub bytes: Vec<u8>,
}

/// A chain's own scratch space, for hints that make recovery cheap: the checkpoint an Ethereum
/// store was last rebuilt from, the Celestia height an Eden store sits at, Eden's captured tree
/// proofs. Only ever hints: every one is checked against the ISM before it is used, so a stale
/// or missing file costs a search, never a wrong answer.
#[derive(Clone)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self { dir }
    }

    pub fn read(&self, name: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.join(name)).ok()?;
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    pub fn write(&self, name: &str, contents: &str) {
        if let Err(e) = std::fs::write(self.dir.join(name), contents) {
            tracing::debug!(name, error = %e, "could not write a cache hint");
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

/// The tree's size at both ends of a step, read through the enclave's own `merkle_tree` on the
/// proofs just fetched. So a bad proof fails here, before an enclave round trip, and the counts
/// are exactly the ones the enclave will see.
pub fn leaves(
    chain: &tee_node::origin::Chain,
    snapshot: &Value,
    trusted_root: [u8; 32],
    tree: &Value,
    head_root: alloy_primitives::B256,
) -> Result<Range<u32>> {
    let before = chain
        .origin
        .merkle_tree(snapshot.clone(), trusted_root.into())?
        .tree
        .count;
    let after = chain
        .origin
        .merkle_tree(tree.clone(), head_root)?
        .tree
        .count;
    anyhow::ensure!(
        before <= after,
        "the tree shrank from {before} to {after} leaves"
    );
    Ok(before..after)
}
