//! What Arbitrum and Base share as origins: both are Ethereum's step plus a proof reading the
//! L2's root out of L1 storage, and both end at an L2 block whose tree is proven the same way.

use alloy_primitives::{Address, B256};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tee_node::origin::Chain;
use tee_node::state::IsmState;

use super::{Ethereum, L1Step};
use crate::evm::{hex_number, quantity, Rpc};
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

pub struct L2 {
    pub l1: Ethereum,
    pub rpc: Rpc,
    mailbox: Address,
    hook: Address,
}

impl L2 {
    pub fn new(config: Config, l1: super::Config, cache: Cache) -> Result<Self> {
        Ok(Self {
            // The L1 store belongs to this L2's ISM, so its hints go in this chain's cache.
            l1: Ethereum::new(l1, cache)?,
            rpc: Rpc::new(&config.rpc, config.logs_rpc.as_deref()),
            mailbox: config.mailbox,
            hook: config.merkle_tree_hook,
        })
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
