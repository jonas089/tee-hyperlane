//! What Base and Arbitrum share: recent blocks their sequencer signed, and tree proofs at them.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tee_node::origin::Chain;
use tee_node::state::IsmState;
use tracing::debug;

use crate::origin::evm::{hex_number, Rpc};
use crate::origin::{self, Message, Step};

/// `[chains.<name>]` for Base and Arbitrum.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub domain: u32,
    /// Must serve `eth_getProof` at the ISM's trusted height, which can be days old: an archive.
    pub rpc: String,
    /// A free endpoint for logs and reads near the head. Archive plans on free tiers cap
    /// `eth_getLogs` at ten blocks, and every tick reads near the head.
    pub logs_rpc: Option<String>,
    /// Where transactions and ISM reads go when this chain is a destination, if not `rpc`.
    pub send_rpc: Option<String>,
    pub mailbox: Address,
    pub merkle_tree_hook: Address,
    /// Base: the UDP port p2p discovery listens on. Any free port works, since peers are only
    /// dialled out; unset picks one.
    #[serde(default)]
    pub p2p_port: u16,
    /// Arbitrum: the sequencer feed.
    pub feed: Option<String>,
}

/// How many recent signed blocks to keep; older ones are never the best choice.
const KEPT: usize = 512;
/// How many of the newest signed blocks to try before giving up for this tick: the RPC can lag
/// the sequencer by a block or two.
pub const TRIES: usize = 4;
/// Past this with nothing from the listener, the source is reported as broken.
pub const SILENT_AFTER: Duration = Duration::from_secs(120);

/// A block the sequencer signed, checked the way the enclave checks it.
#[derive(Clone)]
pub struct SignedHead {
    pub height: u64,
    pub state_root: B256,
    pub timestamp: u64,
    /// The enclave's input for this chain.
    pub input: Value,
}

/// The newest signed items by height (a block, or a feed message by sequence number), as a
/// listener hands them in.
pub struct Recent<T> {
    items: Mutex<BTreeMap<u64, T>>,
    last: Mutex<Option<Instant>>,
}

impl<T> Default for Recent<T> {
    fn default() -> Self {
        Self {
            items: Mutex::new(BTreeMap::new()),
            last: Mutex::new(None),
        }
    }
}

impl<T: Clone> Recent<T> {
    pub fn insert(&self, key: u64, item: T) {
        let mut items = self.items.lock().unwrap_or_else(|p| p.into_inner());
        items.insert(key, item);
        while items.len() > KEPT {
            items.pop_first();
        }
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
    }

    /// Up to `n` items above `key`, newest first.
    pub fn above(&self, key: u64, n: usize) -> Vec<T> {
        let items = self.items.lock().unwrap_or_else(|p| p.into_inner());
        items
            .range(key.saturating_add(1)..)
            .rev()
            .take(n)
            .map(|(_, h)| h.clone())
            .collect()
    }

    /// Fails once nothing has arrived for `SILENT_AFTER`, so a dead source shows as a failing
    /// route rather than a quiet one.
    pub fn check_alive(&self, what: &str) -> Result<()> {
        let last = *self.last.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(silent) = last.map(|t| t.elapsed()).filter(|s| *s > SILENT_AFTER) {
            anyhow::bail!("nothing from the {what} in {}s", silent.as_secs());
        }
        Ok(())
    }

    /// Wait until something arrives, for a genesis state.
    pub async fn first(&self, wait: Duration) -> Result<T> {
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if let Some(item) = self.above(0, 1).pop() {
                return Ok(item);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        anyhow::bail!("nothing arrived within {}s", wait.as_secs())
    }
}

pub struct Sequenced {
    chain: &'static Chain,
    tree_slot: u64,
    rpc: Rpc,
    mailbox: Address,
    hook: Address,
    /// The tree proof at the ISM's trusted height, which only changes when the ISM moves. Read
    /// from the archive, so it is the one metered call, made once per advance.
    snapshot: Mutex<Option<(u64, Value)>>,
}

impl Sequenced {
    pub fn new(chain: &'static Chain, tree_slot: u64, e: &Config) -> Self {
        Self {
            chain,
            tree_slot,
            rpc: Rpc::new(&e.rpc, e.logs_rpc.as_deref()),
            mailbox: e.mailbox,
            hook: e.merkle_tree_hook,
            snapshot: Mutex::new(None),
        }
    }

    pub fn rpc(&self) -> &Rpc {
        &self.rpc
    }

    /// A step to the newest of `heads` (newest first, all above the trusted height) that the
    /// RPC can prove the tree at.
    pub async fn step(&self, trusted: &IsmState, heads: &[SignedHead]) -> Result<Step> {
        let Some(newest) = heads.first() else {
            return Ok(Step::idle(trusted.height));
        };
        let snapshot = self.snapshot(trusted).await?;
        let mut last = None;
        for head in heads {
            let tree = match self
                .rpc
                .recent_tree_proof(self.hook, self.tree_slot, head.height)
                .await
            {
                Ok(tree) => tree,
                Err(e) => {
                    last = Some(e);
                    continue;
                }
            };
            match origin::leaves(
                self.chain,
                &snapshot,
                trusted.state_root,
                &tree,
                head.state_root,
            ) {
                Ok(leaves) => {
                    return Ok(Step {
                        head: head.height,
                        leaves,
                        chain: self.chain.name,
                        input: head.input.clone(),
                        tree,
                        tree_snapshot: snapshot,
                        tree_address: tee_node::evm::padded(self.hook),
                    })
                }
                Err(e) => {
                    debug!(height = head.height, error = %e, "the RPC does not prove this signed block yet");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("no signed block to prove")))
            .with_context(|| format!("proving the tree at signed block {}", newest.height))
    }

    async fn snapshot(&self, trusted: &IsmState) -> Result<Value> {
        if let Some((h, proof)) = &*self.snapshot.lock().unwrap_or_else(|p| p.into_inner()) {
            if *h == trusted.height {
                return Ok(proof.clone());
            }
        }
        let proof = self
            .rpc
            .tree_proof(self.hook, self.tree_slot, json!(hex_number(trusted.height)))
            .await
            .context("reading the tree at the trusted height; `rpc` must be an archive")?;
        *self.snapshot.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((trusted.height, proof.clone()));
        Ok(proof)
    }

    pub async fn index(&self, from: u64, to: u64) -> Result<Vec<Message>> {
        self.rpc
            .dispatched(self.mailbox, self.hook, from + 1, to)
            .await
    }

    /// A genesis state at a signed block. These chains have no light client, so the store
    /// commitment is zero and stays so.
    pub fn genesis(&self, head: &SignedHead, identity: [u8; 32]) -> IsmState {
        IsmState {
            state_root: head.state_root.0,
            origin_domain: self.chain.domain,
            height: head.height,
            timestamp: head.timestamp,
            lc_store_commit: [0; 32],
            identity_digest: identity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_keeps_the_newest_and_answers_newest_first() {
        let recent = Recent::default();
        assert!(
            recent.check_alive("test").is_ok(),
            "nothing yet is not silence"
        );
        for h in 0..(KEPT as u64 + 10) {
            recent.insert(h, h);
        }
        assert_eq!(
            recent.above(100, 3),
            [KEPT as u64 + 9, KEPT as u64 + 8, KEPT as u64 + 7]
        );
        assert_eq!(
            recent.above(5, KEPT + 50).len(),
            KEPT,
            "the oldest are dropped"
        );
        assert!(recent.above(KEPT as u64 + 9, 3).is_empty());
        assert!(recent.above(u64::MAX, 3).is_empty());
    }
}
