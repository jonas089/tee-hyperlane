//! Base as an origin: finding the proof that reads its root out of L1, which the enclave's
//! `ethereum::base::Base` verifies.

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tee_node::ethereum::base::{
    ANCHOR_GAME_SLOT, ANCHOR_STATE_REGISTRY, BASE, ROOT_CLAIM_OFFSET, TREE_SLOT,
};
use tee_node::state::IsmState;

use super::l2_shared::L2;
use crate::origin::evm::{account, hex_number};
use crate::origin::{Indexer, Message, Step};

/// OP Stack's `L2ToL1MessagePasser`, whose storage root is part of the output root.
const MESSAGE_PASSER: &str = "0x4200000000000000000000000000000000000016";

pub struct Base(pub L2);

impl Base {
    /// Read Base's root out of L1 at `l1_block`: the registry's anchor game, the game's code
    /// and claimed root, and the output root's preimage from Base itself. Returns the enclave's
    /// `RootProof` and the L2 block it names.
    async fn root_proof(&self, l1_block: u64) -> Result<(Value, Value, B256)> {
        let (l1, l2) = (self.0.l1.history(), &self.0.rpc);
        let at = hex_number(l1_block);

        let slot = B256::from(U256::from(ANCHOR_GAME_SLOT));
        let registry = l1
            .call("eth_getProof", json!([ANCHOR_STATE_REGISTRY, [slot], at]))
            .await
            .context("eth_getProof on the anchor state registry")?;
        let anchor = &registry["storageProof"][0];
        let slot_value: U256 = anchor["value"]
            .as_str()
            .context("anchorGame value")?
            .parse()?;
        let game = Address::from_slice(&slot_value.to_be_bytes::<32>()[12..]);
        let game_proof = l1
            .call("eth_getProof", json!([game, [], at]))
            .await
            .context("eth_getProof on the anchor game")?;
        let code: Bytes = l1
            .call("eth_getCode", json!([game, at]))
            .await?
            .as_str()
            .context("eth_getCode")?
            .parse()?;
        anyhow::ensure!(
            code.len() >= ROOT_CLAIM_OFFSET + 32,
            "game code is too short to hold rootClaim"
        );

        // Which L2 block the anchor is at, then that block's output root preimage.
        let anchor_root = self.anchor_root(json!(at)).await?;
        anyhow::ensure!(
            anchor_root.len() >= 64,
            "getAnchorRoot returned {} bytes",
            anchor_root.len()
        );
        let number = U256::from_be_slice(&anchor_root[32..64]).to::<u64>();
        let block = l2.block(number).await?;
        let passer = l2
            .call(
                "eth_getProof",
                json!([MESSAGE_PASSER, [], hex_number(number)]),
            )
            .await
            .context("eth_getProof on the message passer; the Base endpoint must be an archive")?;
        let block_hash: B256 = block["hash"].as_str().context("block hash")?.parse()?;
        let (header_rlp, l2_block) = l2.header_rlp(block_hash).await?;

        let root_proof = json!({
            "registry_account": account(&registry),
            "registry_account_proof": registry["accountProof"],
            "anchor_game_proof": anchor["proof"],
            "anchor_game_slot_value": slot_value,
            "game_account": account(&game_proof),
            "game_account_proof": game_proof["accountProof"],
            "game_code": code,
            "output_root": {
                "version": B256::ZERO,
                "state_root": block["stateRoot"],
                "message_passer_storage_root": passer["storageHash"],
                "latest_block_hash": block_hash,
            },
            "l2_header_rlp": format!("0x{}", hex::encode(header_rlp)),
        });
        Ok((root_proof, l2_block, keccak256(&anchor_root)))
    }

    /// `getAnchorRoot()` at `block`: the anchored output root and its L2 block number.
    async fn anchor_root(&self, block: Value) -> Result<Vec<u8>> {
        let selector = format!("0x{}", hex::encode(&keccak256(b"getAnchorRoot()")[..4]));
        let call = json!([{ "to": ANCHOR_STATE_REGISTRY, "data": selector }, block]);
        Ok(hex::decode(
            self.0
                .l1
                .history()
                .call("eth_call", call)
                .await?
                .as_str()
                .context("eth_call")?
                .trim_start_matches("0x"),
        )?)
    }

    /// The anchor at L1's finalized block, as the marker the proof would carry.
    async fn marker(&self) -> Result<B256> {
        Ok(keccak256(self.anchor_root(json!("finalized")).await?))
    }
}

#[async_trait]
impl Indexer for Base {
    async fn gather(&self, trusted: &IsmState) -> Result<Step> {
        if let Some(step) = self.0.unchanged(trusted, self.marker()).await? {
            return Ok(step);
        }
        let Some(l1) = self.0.l1.l1_step(trusted).await? else {
            let step = Step::idle(trusted.height);
            self.0.checked(trusted, None, &step);
            return Ok(step);
        };
        let (proof, l2_block, anchor) = self.root_proof(l1.block).await?;
        let step = self
            .0
            .step(&BASE, TREE_SLOT, trusted, l1, proof, &l2_block)
            .await?;
        self.0.checked(trusted, Some(anchor), &step);
        Ok(step)
    }

    async fn index(&self, from: u64, to: u64) -> Result<Vec<Message>> {
        self.0.index(from, to).await
    }

    async fn bootstrap(&self, identity: [u8; 32], _height: Option<u64>) -> Result<IsmState> {
        let store = self.0.l1.genesis_store().await?;
        let (_, l2_block, _) = self.root_proof(store.root()?.height).await?;
        L2::genesis(&BASE, &store, &l2_block, identity)
    }
}
