//! What Arbitrum and Base share as origins: both are Ethereum's step plus a proof reading the
//! L2's root out of L1 storage, and both end at an L2 block whose tree is proven the same way.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tee_node::origin::Chain;
use tee_node::state::IsmState;

use super::{Ethereum, L1Step};
use crate::origin::evm::{hex_number, quantity, Rpc};
use crate::origin::{self, Cache, Message, Step};

/// `[chains.<name>]` for an L2.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub domain: u32,
    /// The Ethereum chain, by name, whose light client secures this one.
    pub l1: String,
    /// Must serve state at the *confirmed* L2 block, thousands behind head: an archive.
    pub rpc: String,
    /// Where to read dispatch logs, when `rpc` caps `eth_getLogs` (free archive plans cap it at
    /// ten blocks, and a confirmed head moves thousands at a time).
    pub logs_rpc: Option<String>,
    /// Where transactions and ISM reads go when this chain is a destination, if not `rpc`.
    /// Proof reads want an archive; relaying wants a node that is not rate-limited or metered.
    pub send_rpc: Option<String>,
    pub mailbox: Address,
    pub merkle_tree_hook: Address,
}

/// How often an L2 route looks at L1 for a new settled root. Settlement moves roughly hourly
/// on both networks and a transfer waits hours to days for it, so a minute costs nothing in
/// latency, and in between a pass makes no call at all.
pub const CHECK_EVERY: Duration = Duration::from_secs(60);

pub struct L2 {
    pub l1: Ethereum,
    pub rpc: Rpc,
    /// This chain's hints, shared with the L1 store's.
    pub cache: Cache,
    mailbox: Address,
    hook: Address,
    last: Mutex<Option<Checked>>,
}

/// What the last look at L1 found: the settled root's marker the full pass used, the ISM
/// height it ran against, and the head it reported.
#[derive(Clone, Copy)]
struct Checked {
    at: Instant,
    marker: Option<B256>,
    trusted_height: u64,
    head: u64,
}

impl L2 {
    pub fn new(config: Config, l1: super::Config, cache: Cache) -> Result<Self> {
        Ok(Self {
            // The L1 store belongs to this L2's ISM, so its hints go in this chain's cache.
            l1: Ethereum::new(l1, cache.clone())?,
            cache,
            rpc: Rpc::new(&config.rpc, config.logs_rpc.as_deref()),
            mailbox: config.mailbox,
            hook: config.merkle_tree_hook,
            last: Mutex::new(None),
        })
    }

    /// Poll on change. `Some(step)` when this pass can end without the proofs: it is within
    /// `CHECK_EVERY` of the last look, or L1's settled root (`marker`, read at the finalized
    /// block) is the one the last full pass already used against this ISM height. `marker` is
    /// only called when the minute is up.
    pub async fn unchanged<F>(&self, trusted: &IsmState, marker: F) -> Result<Option<Step>>
    where
        F: std::future::Future<Output = Result<B256>>,
    {
        let last = *self.last.lock().unwrap_or_else(|p| p.into_inner());
        let Some(last) = last.filter(|l| l.trusted_height == trusted.height) else {
            return Ok(None);
        };
        if last.at.elapsed() < CHECK_EVERY {
            return Ok(Some(Step::idle(last.head)));
        }
        let now = marker.await?;
        let mut guard = self.last.lock().unwrap_or_else(|p| p.into_inner());
        if last.marker == Some(now) {
            if let Some(l) = guard.as_mut() {
                l.at = Instant::now();
            }
            return Ok(Some(Step::idle(last.head)));
        }
        Ok(None)
    }

    /// Remember a full pass that found nothing to attest, against the marker its proof was read
    /// at. A pass with leaves is not remembered, so a failed attestation is retried in full.
    ///
    /// The marker is the one the proof used, not the one `unchanged` read: the proof is taken at
    /// the light client's finalized block, which can trail the endpoint's, and recording the
    /// newer marker would skip the root that block has not reached yet.
    pub fn checked(&self, trusted: &IsmState, marker: Option<B256>, step: &Step) {
        let entry = step.leaves.is_empty().then(|| Checked {
            at: Instant::now(),
            marker,
            trusted_height: trusted.height,
            head: step.head,
        });
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = entry;
    }

    /// Finish a step, given Ethereum's half, the proof of the L2's root and the L2 block it
    /// confirms.
    pub async fn step(
        &self,
        chain: &'static Chain,
        tree_slot: u64,
        trusted: &IsmState,
        l1: L1Step,
        root_proof: Value,
        l2_block: &Value,
    ) -> Result<Step> {
        let head = quantity(&l2_block["number"])?;
        if head <= trusted.height {
            return Ok(Step::idle(head));
        }
        let head_root: B256 = l2_block["stateRoot"]
            .as_str()
            .context("stateRoot")?
            .parse()?;
        let tree = self
            .rpc
            .tree_proof(self.hook, tree_slot, json!(hex_number(head)))
            .await?;
        let snapshot = self
            .rpc
            .tree_proof(self.hook, tree_slot, json!(hex_number(trusted.height)))
            .await?;
        Ok(Step {
            head,
            leaves: origin::leaves(chain, &snapshot, trusted.state_root, &tree, head_root)?,
            chain: chain.name,
            input: json!({ "ethereum": l1.input, "proof": root_proof }),
            tree,
            tree_snapshot: snapshot,
            tree_address: tee_node::evm::padded(self.hook),
        })
    }

    pub async fn index(&self, from: u64, to: u64) -> Result<Vec<Message>> {
        self.rpc
            .dispatched(self.mailbox, self.hook, from + 1, to)
            .await
    }

    /// A genesis state at the L2 block Ethereum's genesis store confirms.
    pub fn genesis(
        chain: &Chain,
        store: &tee_node::ethereum::EthereumStore,
        l2_block: &Value,
        identity: [u8; 32],
    ) -> Result<IsmState> {
        Ok(IsmState {
            state_root: l2_block["stateRoot"]
                .as_str()
                .context("stateRoot")?
                .parse::<B256>()?
                .0,
            origin_domain: chain.domain,
            height: quantity(&l2_block["number"])?,
            timestamp: quantity(&l2_block["timestamp"])?,
            lc_store_commit: store.commitment(),
            identity_digest: identity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l2() -> L2 {
        let l1: super::super::Config = toml::from_str(
            r#"domain = 1
rpc = "http://127.0.0.1:1"
beacon_rpc = "http://127.0.0.1:1"
mailbox = "0x0000000000000000000000000000000000000001"
merkle_tree_hook = "0x0000000000000000000000000000000000000001""#,
        )
        .unwrap();
        let config: Config = toml::from_str(
            r#"domain = 2
l1 = "l1"
rpc = "http://127.0.0.1:1"
mailbox = "0x0000000000000000000000000000000000000001"
merkle_tree_hook = "0x0000000000000000000000000000000000000001""#,
        )
        .unwrap();
        let dir = std::env::temp_dir().join("teeism-l2-cadence");
        L2::new(config, l1, Cache::new(dir)).unwrap()
    }

    fn trusted(height: u64) -> IsmState {
        IsmState {
            state_root: [0; 32],
            origin_domain: 2,
            height,
            timestamp: 0,
            lc_store_commit: [0; 32],
            identity_digest: [0; 32],
        }
    }

    fn marker(b: u8) -> B256 {
        B256::repeat_byte(b)
    }

    #[tokio::test]
    async fn a_pass_is_skipped_only_while_nothing_it_depends_on_moved() {
        let l2 = l2();
        let never = async { panic!("the marker must not be read inside the minute") };
        assert!(
            l2.unchanged(&trusted(5), async { Ok(marker(1)) })
                .await
                .unwrap()
                .is_none(),
            "no pass yet"
        );

        l2.checked(&trusted(5), Some(marker(1)), &Step::idle(9));
        let skipped = l2.unchanged(&trusted(5), never).await.unwrap().unwrap();
        assert_eq!(skipped.head, 9, "reports the head the full pass found");

        assert!(
            l2.unchanged(&trusted(6), async { Ok(marker(1)) })
                .await
                .unwrap()
                .is_none(),
            "the ISM moved"
        );

        // Past the minute: the same marker skips, a new one runs the full pass.
        l2.last.lock().unwrap().as_mut().unwrap().at -= CHECK_EVERY;
        assert!(l2
            .unchanged(&trusted(5), async { Ok(marker(1)) })
            .await
            .unwrap()
            .is_some());
        l2.last.lock().unwrap().as_mut().unwrap().at -= CHECK_EVERY;
        assert!(l2
            .unchanged(&trusted(5), async { Ok(marker(2)) })
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn a_pass_with_leaves_is_not_remembered() {
        let l2 = l2();
        let mut step = Step::idle(9);
        step.leaves = 3..4;
        l2.checked(&trusted(5), Some(marker(1)), &step);
        assert!(l2
            .unchanged(&trusted(5), async { Ok(marker(1)) })
            .await
            .unwrap()
            .is_none());
    }
}
