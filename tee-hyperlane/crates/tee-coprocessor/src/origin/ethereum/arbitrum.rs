//! Arbitrum Sepolia as an origin: finding the proof that reads its confirmed root out of L1,
//! which the enclave's `ethereum::arbitrum::Arbitrum` verifies.

use alloy_primitives::{keccak256, B256, U256};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tee_node::ethereum::arbitrum::{
    ARBITRUM, ASSERTIONS_SLOT, LATEST_CONFIRMED_SLOT, ROLLUP, TREE_SLOT,
};
use tee_node::state::IsmState;

use super::l2_shared::L2;
use crate::origin::evm::{account, hex_number};
use crate::origin::{Indexer, Message, Step};

/// `AssertionCreated(bytes32 indexed assertionHash, bytes32 indexed parentAssertionHash, ...)`.
const ASSERTION_CREATED_TOPIC: &str =
    "0x901c3aee23cf4478825462caaab375c606ab83516060388344f0650340753630";
/// Where the after-state and the inbox accumulator sit in the event's data, in 32-byte words.
const AFTER_STATE_WORD: usize = 13;
const INBOX_ACC_WORD: usize = 19;
/// How far back to look for the confirmed assertion's creation. BoLD confirms within days;
/// five thousand L1 blocks is well past that.
const ASSERTION_SEARCH_BLOCKS: u64 = 5_000;

pub struct Arbitrum(pub L2);

impl Arbitrum {
    /// Read Arbitrum's confirmed root out of L1 at `l1_block`: which assertion is confirmed,
    /// its status, its preimage from the `AssertionCreated` event, and the L2 header it names.
    /// Returns the enclave's `RootProof` and the L2 block it confirms.
    async fn root_proof(&self, l1_block: u64) -> Result<(Value, Value)> {
        let l1 = self.0.l1.history();
        let at = hex_number(l1_block);
        let latest_slot = B256::from(U256::from(LATEST_CONFIRMED_SLOT));
        let confirmed: B256 = l1
            .call("eth_getStorageAt", json!([ROLLUP, latest_slot, at]))
            .await?
            .as_str()
            .context("eth_getStorageAt")?
            .parse()?;
        let mut node_slot = [0u8; 64];
        node_slot[..32].copy_from_slice(confirmed.as_slice());
        node_slot[56..].copy_from_slice(&ASSERTIONS_SLOT.to_be_bytes());
        let proof = l1
            .call(
                "eth_getProof",
                json!([ROLLUP, [latest_slot, B256::from(keccak256(node_slot))], at]),
            )
            .await
            .context("eth_getProof on the rollup; the L1 block may be outside the proof window")?;
        let slots = proof["storageProof"].as_array().context("storageProof")?;
        anyhow::ensure!(
            slots.len() == 2,
            "expected two storage proofs from the rollup"
        );

        // The preimage of the confirmed assertion, from the event that created it.
        let head = l1.block_number().await?;
        let logs = l1
            .call(
                "eth_getLogs",
                json!([{
                    "address": ROLLUP,
                    "topics": [ASSERTION_CREATED_TOPIC, confirmed],
                    "fromBlock": hex_number(head.saturating_sub(ASSERTION_SEARCH_BLOCKS)),
                    "toBlock": "latest",
                }]),
            )
            .await?;
        let log = logs
            .as_array()
            .and_then(|l| l.first())
            .with_context(|| format!("no AssertionCreated for {confirmed} in the last {ASSERTION_SEARCH_BLOCKS} L1 blocks"))?;
        let data = log["data"]
            .as_str()
            .context("log data")?
            .trim_start_matches("0x");
        let word = |i: usize| -> Result<B256> {
            Ok(format!(
                "0x{}",
                data.get(i * 64..i * 64 + 64)
                    .context("AssertionCreated data too short")?
            )
            .parse()?)
        };
        let small = |i: usize| -> Result<u64> { Ok(U256::from_be_bytes(word(i)?.0).to::<u64>()) };
        let l2_block_hash = word(AFTER_STATE_WORD)?;
        let (header_rlp, l2_block) = self.0.rpc.header_rlp(l2_block_hash).await?;

        let root_proof = json!({
            "account": account(&proof),
            "account_proof": proof["accountProof"],
            "latest_confirmed_proof": slots[0]["proof"],
            "assertion_node_proof": slots[1]["proof"],
            "assertion_node_slot_value": slots[1]["value"],
            "prev_assertion_hash": log["topics"][2],
            "after_state": {
                "l2_block_hash": l2_block_hash,
                "send_root": word(AFTER_STATE_WORD + 1)?,
                "inbox_position": small(AFTER_STATE_WORD + 2)?,
                "position_in_message": small(AFTER_STATE_WORD + 3)?,
                "machine_status": small(AFTER_STATE_WORD + 4)?,
                "end_history_root": word(AFTER_STATE_WORD + 5)?,
            },
            "inbox_accumulator": word(INBOX_ACC_WORD)?,
            "l2_header_rlp": format!("0x{}", hex::encode(header_rlp)),
        });
        Ok((root_proof, l2_block))
    }
}

#[async_trait]
impl Indexer for Arbitrum {
    async fn gather(&self, trusted: &IsmState) -> Result<Step> {
        let Some(l1) = self.0.l1.l1_step(trusted).await? else {
            return Ok(Step::idle(trusted.height));
        };
        let (proof, l2_block) = self.root_proof(l1.block).await?;
        self.0
            .step(&ARBITRUM, TREE_SLOT, trusted, l1, proof, &l2_block)
            .await
    }

    async fn index(&self, from: u64, to: u64) -> Result<Vec<Message>> {
        self.0.index(from, to).await
    }

    async fn bootstrap(&self, identity: [u8; 32], _height: Option<u64>) -> Result<IsmState> {
        let store = self.0.l1.genesis_store().await?;
        let (_, l2_block) = self.root_proof(store.root()?.height).await?;
        L2::genesis(&ARBITRUM, &store, &l2_block, identity)
    }
}
