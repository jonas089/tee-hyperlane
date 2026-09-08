//! Ethereum consensus: a sync-committee light client.
//!
//! The enclave verifies BLS aggregate signatures from the sync committee against a
//! weak-subjectivity checkpoint, so the beacon and execution RPCs are pure data sources -
//! if they lie, verification fails rather than producing a wrong root. This is what makes
//! "light node" an honest description rather than "we relayed what one RPC told us".
//!
//! The verification functions come from helios' consensus-core, which is pure: no I/O, no
//! async, operating on a `LightClientStore` we carry in and out. That is exactly the shape a
//! stateless enclave needs.

use helios_consensus_core::consensus_spec::MainnetConsensusSpec;
use helios_consensus_core::types::{FinalityUpdate, Forks, LightClientStore, Update};
use helios_consensus_core::{
    apply_finality_update, apply_update, verify_finality_update, verify_update,
};
use sha2::{Digest, Sha256};
use ssz::Encode;
use tree_hash::TreeHash;

use super::AttestedRoot;

/// Sepolia uses mainnet consensus parameters, including a 512-key sync committee.
pub type Spec = MainnetConsensusSpec;

/// The light-client state the ISM commits to.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EthereumStore {
    pub store: LightClientStore<Spec>,
    /// Beacon genesis validators root; binds the store to one chain.
    pub genesis_root: alloy_primitives::B256,
    pub genesis_time: u64,
    pub forks: Forks,
}

/// Sync-committee updates, in order, followed by the finality update that moves the head.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct EthereumUpdates {
    pub committee_updates: Vec<Update<Spec>>,
    pub finality_update: Option<FinalityUpdate<Spec>>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum EthereumError {
    #[error("sync committee update {index} rejected: {reason}")]
    UpdateRejected { index: usize, reason: String },
    #[error("finality update rejected: {reason}")]
    FinalityRejected { reason: String },
    #[error("no updates supplied")]
    NoUpdates,
    #[error("finalized header is pre-Capella and carries no execution payload")]
    NoExecutionPayload,
    #[error("finalized head did not advance: still at block {0}")]
    HeadNotAdvanced(u64),
}

/// Advance the light client over the supplied updates.
///
/// `expected_current_slot` is the enclave's view of the current slot. It bounds how far into
/// the future an update may claim to be; it cannot be taken from the update itself.
pub fn verify_ethereum_updates(
    store: &mut EthereumStore,
    updates: &EthereumUpdates,
    expected_current_slot: u64,
) -> Result<(), EthereumError> {
    if updates.committee_updates.is_empty() && updates.finality_update.is_none() {
        return Err(EthereumError::NoUpdates);
    }
    let before = finalized_block_number(store).unwrap_or(0);

    for (index, update) in updates.committee_updates.iter().enumerate() {
        verify_update::<Spec>(
            update,
            expected_current_slot,
            &store.store,
            store.genesis_root,
            &store.forks,
        )
        .map_err(|e| EthereumError::UpdateRejected {
            index,
            reason: e.to_string(),
        })?;
        apply_update::<Spec>(&mut store.store, update);
    }

    if let Some(finality) = &updates.finality_update {
        verify_finality_update::<Spec>(
            finality,
            expected_current_slot,
            &store.store,
            store.genesis_root,
            &store.forks,
        )
        .map_err(|e| EthereumError::FinalityRejected {
            reason: e.to_string(),
        })?;
        apply_finality_update::<Spec>(&mut store.store, finality);
    }

    let after = finalized_block_number(store).ok_or(EthereumError::NoExecutionPayload)?;
    if after <= before {
        return Err(EthereumError::HeadNotAdvanced(after));
    }
    Ok(())
}

/// The execution state root of the finalized beacon head.
///
/// Finalized, not optimistic: an optimistic head can still be reorged, and a bridge that
/// mints against a reorged root loses funds.
pub fn ethereum_root(store: &EthereumStore) -> Result<AttestedRoot, EthereumError> {
    let execution = store
        .store
        .finalized_header
        .execution()
        .map_err(|_| EthereumError::NoExecutionPayload)?;
    Ok(AttestedRoot {
        state_root: *execution.state_root(),
        height: *execution.block_number(),
        timestamp: *execution.timestamp(),
    })
}

/// Commit to everything the next verification depends on.
///
/// The sync committees matter as much as the header: without them a relayer could hand the
/// enclave a different committee and defeat the signature check on the following update.
pub fn commit_ethereum_store(store: &EthereumStore) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"tee-isms/ethereum-store/v1");
    h.update(store.genesis_root.as_slice());
    h.update(store.genesis_time.to_be_bytes());
    h.update(
        store
            .store
            .finalized_header
            .beacon()
            .tree_hash_root()
            .as_slice(),
    );
    // The forks decide which signing domain each update is checked under, so they are part
    // of what the next verification depends on and have to be committed like everything else.
    // Hashed field by field rather than through a derive, so adding a fork to the upstream
    // type is a compile error here rather than a silently uncommitted field.
    for fork in [
        &store.forks.genesis,
        &store.forks.altair,
        &store.forks.bellatrix,
        &store.forks.capella,
        &store.forks.deneb,
        &store.forks.electra,
        &store.forks.fulu,
    ] {
        h.update(fork.epoch.to_be_bytes());
        h.update(fork.fork_version);
    }
    h.update(store.store.current_sync_committee.as_ssz_bytes());
    match &store.store.next_sync_committee {
        Some(next) => {
            h.update([1u8]);
            h.update(next.as_ssz_bytes());
        }
        None => h.update([0u8]),
    }
    h.finalize().into()
}

fn finalized_block_number(store: &EthereumStore) -> Option<u64> {
    store
        .store
        .finalized_header
        .execution()
        .ok()
        .map(|e| *e.block_number())
}
