//! From a verified state root to an authorised batch of Hyperlane message ids.
//!
//! The enclave does this natively rather than in a circuit. An on-chain inclusion proof
//! under a TEE-attested root would add no security - whoever can forge the root can forge a
//! proof under it - so the only thing that matters is that this code is inside the
//! measurement, which it is.

use alloy_primitives::{Address, Bytes, B256};
use hyperlane_types::{
    build_tree_from_slots, get_tree_root, insert_leaf, MerkleTree, MerkleTreeSlots, TREE_DEPTH,
};

use crate::state_proofs::{verify_account_proof, verify_storage_proof, ClaimedAccount, MptError};

/// Everything needed to prove the origin merkle tree out of EVM state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvmTreeProof {
    pub merkle_tree_hook: Address,
    pub account: ClaimedAccount,
    pub account_proof: Vec<Bytes>,
    /// One entry per slot, branch first then count, 33 in total.
    pub storage_proof: Vec<crate::state_proofs::ClaimedSlot>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HyperlaneStateError {
    #[error("a batch must carry at least one message id")]
    EmptyBatch,
    #[error(transparent)]
    Mpt(#[from] MptError),
    #[error("expected {expected} storage proofs, got {got}")]
    WrongSlotCount { expected: usize, got: usize },
    #[error("storage proof {index} is for slot {got}, expected {expected}")]
    SlotMismatch { index: usize, got: B256, expected: B256 },
    #[error("merkle tree count {0} does not fit in uint32")]
    CountNotUint32(u128),
    #[error("replaying {ids} message ids gave root {replayed} at count {count}, but the chain proves {onchain}")]
    ReplayMismatch {
        ids: usize,
        count: u32,
        replayed: String,
        onchain: String,
    },
    #[error("replayed {replayed} leaves but the chain proves {onchain}")]
    CountMismatch { replayed: u32, onchain: u32 },
    #[error("snapshot count {snapshot} exceeds the proven on-chain count {onchain}")]
    SnapshotAhead { snapshot: u32, onchain: u32 },
    #[error("merkle tree is full")]
    TreeFull,
}

/// Prove the origin `MerkleTreeHook`'s incremental tree against an EVM state root.
/// Base storage slot of a merkle tree hook's incremental tree, by origin domain.
///
/// Pinned here rather than taken from the request, for the same reason the L2 anchors are:
/// the enclave should never accept "where to look" from whoever is asking. Deployment
/// specific, and not guessable - Hyperlane's canonical Sepolia hook uses 103 while both L2
/// deployments use 151.
pub fn merkle_tree_base_slot(origin_domain: u32) -> Option<u64> {
    match origin_domain {
        11155111 => Some(103),
        421614 | 84532 => Some(151),
        _ => None,
    }
}

pub fn get_evm_merkle_tree(
    state_root: B256,
    base_slot: u64,
    proof: &EvmTreeProof,
) -> Result<MerkleTree, HyperlaneStateError> {
    let storage_root = verify_account_proof(
        state_root,
        proof.merkle_tree_hook,
        &proof.account,
        &proof.account_proof,
    )?;

    let slots = MerkleTreeSlots::new(base_slot);
    let expected_keys = slots.storage_keys();
    if proof.storage_proof.len() != expected_keys.len() {
        return Err(HyperlaneStateError::WrongSlotCount {
            expected: expected_keys.len(),
            got: proof.storage_proof.len(),
        });
    }

    let mut values = [[0u8; 32]; TREE_DEPTH + 1];
    for (i, claimed) in proof.storage_proof.iter().enumerate() {
        let expected = B256::from(expected_keys[i]);
        if claimed.slot != expected {
            return Err(HyperlaneStateError::SlotMismatch {
                index: i,
                got: claimed.slot,
                expected,
            });
        }
        verify_storage_proof(storage_root, claimed.slot, claimed.value, &claimed.proof)?;
        values[i] = claimed.value.to_be_bytes();
    }

    build_tree_from_slots(&values).map_err(|e| match e {
        hyperlane_types::LayoutError::CountNotUint32(c) => HyperlaneStateError::CountNotUint32(c),
    })
}

/// Confirm that `message_ids` are exactly the leaves between the snapshot and the tree
/// proven out of chain state.
///
/// Replaying onto the snapshot has to land on the on-chain `(branch, count)` exactly.
/// Because that branch is a function of every leaf ever inserted, a snapshot that is wrong,
/// stale or gapped cannot reproduce it - which is why the snapshot itself needs no
/// authentication.
pub fn verify_message_batch(
    snapshot: MerkleTree,
    message_ids: &[[u8; 32]],
    onchain: &MerkleTree,
) -> Result<(), HyperlaneStateError> {
    // An empty batch attests nothing, and attesting nothing is how the bridge gets frozen.
    // The destination allows one batch per state root and the root must change on every
    // update, so a batch that authorises no message still burns that root's only slot. An
    // attacker posting straight to the enclave with the head's own tree as the snapshot makes
    // the replay below the identity function, which passes; repeat it faster than the relayer
    // and no message is ever authorised while user funds stay locked.
    //
    // Only the empty batch can do this. Any non-empty one has to reproduce the on-chain count
    // and root exactly, so it must carry precisely the leaves added since the snapshot - and
    // submitting those is the relayer's job, done for free.
    if message_ids.is_empty() {
        return Err(HyperlaneStateError::EmptyBatch);
    }
    if snapshot.count > onchain.count {
        return Err(HyperlaneStateError::SnapshotAhead {
            snapshot: snapshot.count,
            onchain: onchain.count,
        });
    }
    let mut replayed = snapshot;
    for id in message_ids {
        insert_leaf(&mut replayed, *id).map_err(|_| HyperlaneStateError::TreeFull)?;
    }

    // Compare what the tree *means* - its leaf count and its root - rather than the raw
    // branch array. The two Hyperlane implementations disagree about unused levels:
    // hyperlane-cosmos pre-fills them with the canonical zero hashes, Solidity leaves them
    // zero. Those levels are above the highest set bit of `count`, so they contribute
    // nothing to the root and nothing to whether a message is in the tree. Comparing them
    // would make every Celestia-origin batch fail while proving nothing.
    if replayed.count != onchain.count {
        return Err(HyperlaneStateError::CountMismatch {
            replayed: replayed.count,
            onchain: onchain.count,
        });
    }
    let got = get_tree_root(&replayed);
    let want = get_tree_root(onchain);
    if got != want {
        return Err(HyperlaneStateError::ReplayMismatch {
            ids: message_ids.len(),
            count: replayed.count,
            replayed: hex::encode(got),
            onchain: hex::encode(want),
        });
    }
    Ok(())
}

/// The Hyperlane checkpoint root for a proven tree.
pub fn get_merkle_root(tree: &MerkleTree) -> [u8; 32] {
    get_tree_root(tree)
}
