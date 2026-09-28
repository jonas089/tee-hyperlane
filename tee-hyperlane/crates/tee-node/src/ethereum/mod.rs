//! Ethereum (Sepolia): a sync-committee light client.
//!
//! The enclave checks the sync committee's BLS signatures against a store the ISM already
//! commits to, so the beacon and execution RPCs are data sources only.
//!
//! Arbitrum and Base publish their state into Ethereum, so they live here too: each runs
//! `Ethereum.verify` first and reads its own root out of the L1 state root it returns.

pub mod arbitrum;
pub mod base;
mod l2_shared;

use crate::state::IsmState;
use alloy_primitives::B256;
use helios_consensus_core::consensus_spec::MainnetConsensusSpec;
use helios_consensus_core::types::{FinalityUpdate, Forks, LightClientStore, Update};
use helios_consensus_core::{
    apply_finality_update, apply_update, verify_finality_update, verify_update,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use ssz::Encode;
use tree_hash::TreeHash;

use crate::origin::{self, AttestedRoot, Chain, Head, Origin, Tree};

pub static ETHEREUM: Chain = Chain {
    name: "ethereum",
    domain: 11155111,
    origin: &Ethereum,
};

/// Every chain this module attests.
pub static CHAINS: &[&Chain] = &[&ETHEREUM, &arbitrum::ARBITRUM, &base::BASE];

/// Hyperlane's canonical Sepolia `MerkleTreeHook` keeps its tree from slot 103.
pub const TREE_SLOT: u64 = 103;
/// Sepolia's slot time, fixed since genesis.
const SECONDS_PER_SLOT: u64 = 12;

/// Sepolia uses mainnet consensus parameters, including a 512-key sync committee.
pub type Spec = MainnetConsensusSpec;

pub struct Ethereum;

/// An Ethereum step: the store the ISM committed to, and the updates that move it forward.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Input {
    pub store: EthereumStore,
    pub updates: Updates,
}

/// The light client's state, carried by the coprocessor and pinned by `lc_store_commit`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EthereumStore {
    pub store: LightClientStore<Spec>,
    /// Beacon genesis validators root; binds the store to one chain.
    pub genesis_root: B256,
    pub genesis_time: u64,
    pub forks: Forks,
}

/// Sync-committee updates, needed only when a committee period boundary has passed, then the
/// finality update that moves the head.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Updates {
    pub committee_updates: Vec<Update<Spec>>,
    pub finality_update: Option<FinalityUpdate<Spec>>,
}

impl Origin for Ethereum {
    /// Walk the light client forward to a newer finalized head.
    fn verify(&self, input: Value, trusted: &IsmState) -> anyhow::Result<Head> {
        let Input { mut store, updates } = origin::parse("ethereum input", input)?;
        anyhow::ensure!(
            store.commitment() == trusted.lc_store_commit,
            "supplied light-client store does not match the commitment in the ISM state"
        );
        anyhow::ensure!(
            !updates.committee_updates.is_empty() || updates.finality_update.is_some(),
            "no updates supplied"
        );
        // The current slot bounds how far ahead an update may claim to be. It comes from the
        // store's genesis and the enclave's clock: a caller-named slot is a caller-named clock.
        let slot = origin::now()?.saturating_sub(store.genesis_time) / SECONDS_PER_SLOT;
        let before = store.root().map(|r| r.height).unwrap_or(0);

        // Committee updates only when a sync-committee period boundary has passed, then the
        // finality update that moves the head.
        for (i, update) in updates.committee_updates.iter().enumerate() {
            verify_update::<Spec>(update, slot, &store.store, store.genesis_root, &store.forks)
                .map_err(|e| anyhow::anyhow!("sync committee update {i} rejected: {e}"))?;
            apply_update::<Spec>(&mut store.store, update);
        }
        if let Some(finality) = &updates.finality_update {
            verify_finality_update::<Spec>(
                finality,
                slot,
                &store.store,
                store.genesis_root,
                &store.forks,
            )
            .map_err(|e| anyhow::anyhow!("finality update rejected: {e}"))?;
            apply_finality_update::<Spec>(&mut store.store, finality);
        }

        let root = store.root()?;
        anyhow::ensure!(
            root.height > before,
            "finalized head did not advance: still at block {}",
            root.height
        );
        Ok(Head {
            root: root.state_root,
            height: root.height,
            timestamp: root.timestamp,
            store_commit: store.commitment(),
            attested_at: root.timestamp,
        })
    }

    fn merkle_tree(&self, proof: Value, root: B256) -> anyhow::Result<Tree> {
        crate::evm::read_tree(proof, root, TREE_SLOT)
    }
}

/// What the coprocessor needs too: which head a store is at, and what the ISM commits to.
impl EthereumStore {
    /// The execution state root of the finalized head. Finalized, not optimistic: an
    /// optimistic head can still be reorged, and a bridge minting against it loses funds.
    pub fn root(&self) -> anyhow::Result<AttestedRoot> {
        let execution = self.store.finalized_header.execution().map_err(|_| {
            anyhow::anyhow!("finalized header is pre-Capella and has no execution payload")
        })?;
        Ok(AttestedRoot {
            state_root: *execution.state_root(),
            height: *execution.block_number(),
            timestamp: *execution.timestamp(),
        })
    }

    /// Commit to everything the next verification depends on. The sync committees matter as
    /// much as the header: without them a relayer could swap in a committee of its choosing.
    pub fn commitment(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"tee-isms/ethereum-store/v1");
        h.update(self.genesis_root.as_slice());
        h.update(self.genesis_time.to_be_bytes());
        h.update(
            self.store
                .finalized_header
                .beacon()
                .tree_hash_root()
                .as_slice(),
        );
        // The forks decide which signing domain each update is checked under. Listed field by
        // field, so a fork added upstream is a compile error here, not an uncommitted field.
        let f = &self.forks;
        for fork in [
            &f.genesis,
            &f.altair,
            &f.bellatrix,
            &f.capella,
            &f.deneb,
            &f.electra,
            &f.fulu,
        ] {
            h.update(fork.epoch.to_be_bytes());
            h.update(fork.fork_version);
        }
        h.update(self.store.current_sync_committee.as_ssz_bytes());
        match &self.store.next_sync_committee {
            Some(next) => {
                h.update([1u8]);
                h.update(next.as_ssz_bytes());
            }
            None => h.update([0u8]),
        }
        h.finalize().into()
    }
}
