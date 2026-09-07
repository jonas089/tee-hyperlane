//! End-to-end over live Sepolia state: block stateRoot -> account proof -> 33 storage
//! proofs -> Hyperlane merkle root, all verified rather than trusted.

use alloy_primitives::{Address, Bytes, B256, U256};
use hyperlane_types::{get_message_id, get_tree_root, insert_leaf, HyperlaneMessage, MerkleTree};
use tee_node::hyperlane_state::*;
use tee_node::state_proofs::*;

#[derive(serde::Deserialize)]
struct Fixture {
    block_number: u64,
    state_root: B256,
    address: Address,
    base_slot: u64,
    account: ClaimedAccount,
    account_proof: Vec<Bytes>,
    storage_proof: Vec<ClaimedSlot>,
    expected_root: B256,
    expected_count: u32,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("../testdata/sepolia_hook_proof.json")).unwrap()
}

fn tree_proof(f: &Fixture) -> EvmTreeProof {
    EvmTreeProof {
        merkle_tree_hook: f.address,
        base_slot: f.base_slot,
        account: f.account,
        account_proof: f.account_proof.clone(),
        storage_proof: f.storage_proof.clone(),
    }
}

#[test]
fn live_sepolia_state_root_yields_the_hyperlane_merkle_root() {
    let f = fixture();
    let tree = get_evm_merkle_tree(f.state_root, &tree_proof(&f)).expect("proof must verify");
    assert_eq!(tree.count, f.expected_count);
    assert_eq!(
        B256::from(get_merkle_root(&tree)),
        f.expected_root,
        "reconstructed root disagrees with the hook's root() at block {}",
        f.block_number
    );
}

/// Twelve of the 33 slots are zero, so this path is exercised by the fixture itself:
/// Ethereum stores no zeros, so an unset branch level is proven by exclusion.
#[test]
fn zero_valued_branch_slots_are_proven_by_exclusion() {
    let f = fixture();
    let zeros = f.storage_proof.iter().filter(|s| s.value.is_zero()).count();
    assert!(zeros > 0, "fixture should contain unset branch levels");
    let storage_root = verify_account_proof(
        f.state_root,
        f.address,
        &f.account,
        &f.account_proof,
    )
    .unwrap();
    for s in f.storage_proof.iter().filter(|s| s.value.is_zero()) {
        verify_storage_proof(storage_root, s.slot, s.value, &s.proof)
            .expect("exclusion proof must verify");
    }
}

#[test]
fn a_wrong_state_root_is_rejected() {
    let f = fixture();
    let err = get_evm_merkle_tree(B256::repeat_byte(0xab), &tree_proof(&f)).unwrap_err();
    assert!(matches!(err, HyperlaneStateError::Mpt(MptError::AccountProof { .. })));
}

#[test]
fn a_tampered_slot_value_is_rejected() {
    let f = fixture();
    let mut p = tree_proof(&f);
    p.storage_proof[0].value = U256::from(1);
    let err = get_evm_merkle_tree(f.state_root, &p).unwrap_err();
    assert!(matches!(err, HyperlaneStateError::Mpt(MptError::StorageProof { .. })));
}

/// Claiming a real value for the wrong slot must not slip through.
#[test]
fn slots_must_arrive_in_the_expected_order() {
    let f = fixture();
    let mut p = tree_proof(&f);
    p.storage_proof.swap(0, 1);
    let err = get_evm_merkle_tree(f.state_root, &p).unwrap_err();
    assert!(matches!(err, HyperlaneStateError::SlotMismatch { index: 0, .. }));
}

#[test]
fn a_wrong_base_slot_is_rejected() {
    let f = fixture();
    let mut p = tree_proof(&f);
    p.base_slot = 151; // celestia-zkevm's layout, wrong for this deployment
    let err = get_evm_merkle_tree(f.state_root, &p).unwrap_err();
    assert!(matches!(err, HyperlaneStateError::SlotMismatch { index: 0, .. }));
}

#[test]
fn a_short_storage_proof_is_rejected() {
    let f = fixture();
    let mut p = tree_proof(&f);
    p.storage_proof.pop();
    assert!(matches!(
        get_evm_merkle_tree(f.state_root, &p),
        Err(HyperlaneStateError::WrongSlotCount { expected: 33, got: 32 })
    ));
}

// ---- message batch authorisation ----

fn msg(nonce: u32) -> HyperlaneMessage {
    HyperlaneMessage {
        version: 3,
        nonce,
        origin: 11155111,
        sender: [1u8; 32],
        destination: 1297040200,
        recipient: [2u8; 32],
        body: vec![7u8; 64],
    }
}

fn tree_of(n: u32) -> (MerkleTree, Vec<[u8; 32]>) {
    let mut t = MerkleTree::default();
    let mut ids = Vec::new();
    for i in 0..n {
        let id = get_message_id(&msg(i));
        insert_leaf(&mut t, id).unwrap();
        ids.push(id);
    }
    (t, ids)
}

#[test]
fn the_exact_message_batch_is_accepted() {
    let (snapshot, _) = tree_of(5);
    let (onchain, all) = tree_of(9);
    assert_eq!(verify_message_batch(snapshot, &all[5..], &onchain), Ok(()));
}

#[test]
fn an_empty_batch_is_accepted_when_nothing_was_dispatched() {
    let (t, _) = tree_of(5);
    assert_eq!(verify_message_batch(t, &[], &t), Ok(()));
}

#[test]
fn a_forged_message_id_is_rejected() {
    let (snapshot, _) = tree_of(5);
    let (onchain, all) = tree_of(9);
    let mut forged = all[5..].to_vec();
    forged[1] = [0xde; 32];
    assert!(matches!(
        verify_message_batch(snapshot, &forged, &onchain),
        Err(HyperlaneStateError::ReplayMismatch { .. })
    ));
}

#[test]
fn a_gap_in_the_batch_is_rejected() {
    let (snapshot, _) = tree_of(5);
    let (onchain, all) = tree_of(9);
    let gapped: Vec<_> = all[5..].iter().skip(1).copied().collect();
    assert!(matches!(
        verify_message_batch(snapshot, &gapped, &onchain),
        Err(HyperlaneStateError::CountMismatch { .. })
    ));
}

#[test]
fn reordering_the_batch_is_rejected() {
    let (snapshot, _) = tree_of(5);
    let (onchain, all) = tree_of(9);
    let mut swapped = all[5..].to_vec();
    swapped.swap(0, 1);
    assert!(matches!(
        verify_message_batch(snapshot, &swapped, &onchain),
        Err(HyperlaneStateError::ReplayMismatch { .. })
    ));
}

/// A snapshot that already contains messages the chain has not yet seen would let a
/// relayer skip a batch silently.
#[test]
fn a_snapshot_ahead_of_chain_state_is_rejected() {
    let (ahead, _) = tree_of(9);
    let (onchain, _) = tree_of(5);
    assert!(matches!(
        verify_message_batch(ahead, &[], &onchain),
        Err(HyperlaneStateError::SnapshotAhead { snapshot: 9, onchain: 5 })
    ));
}

/// The tree root must be a function of the whole history, not just the new batch.
#[test]
fn a_wrong_snapshot_cannot_reproduce_the_onchain_tree() {
    let (_, all) = tree_of(9);
    let wrong = MerkleTree::default();
    let (onchain, _) = tree_of(9);
    assert!(matches!(
        verify_message_batch(wrong, &all[5..], &onchain),
        Err(HyperlaneStateError::CountMismatch { .. })
    ));
    // ...whereas the true snapshot does.
    let (snapshot, _) = tree_of(5);
    assert_eq!(verify_message_batch(snapshot, &all[5..], &onchain), Ok(()));
    assert_eq!(get_tree_root(&onchain), get_merkle_root(&onchain));
}


/// hyperlane-cosmos pre-fills a tree's unused branch levels with the canonical zero hashes;
/// Solidity leaves them zero. Those levels sit above the highest set bit of `count`, so they
/// change neither the root nor which messages are in the tree.
///
/// Comparing raw structs would therefore reject every Celestia-origin batch while proving
/// nothing - which is exactly what happened against live mocha-5 before this was fixed.
#[test]
fn unused_branch_levels_may_differ_between_implementations() {
    use hyperlane_types::zero_hashes;

    let (mut solidity_style, ids) = tree_of(3);
    let mut cosmos_style = solidity_style;
    let zeros = zero_hashes();
    // Only levels the tree has not reached yet.
    for level in 2..hyperlane_types::TREE_DEPTH {
        cosmos_style.branch[level] = zeros[level];
    }

    assert_ne!(solidity_style, cosmos_style, "the structs really do differ");
    assert_eq!(
        get_tree_root(&solidity_style),
        get_tree_root(&cosmos_style),
        "but they mean the same tree"
    );

    let snapshot = MerkleTree::default();
    assert_eq!(verify_message_batch(snapshot, &ids, &cosmos_style), Ok(()));
    assert_eq!(verify_message_batch(snapshot, &ids, &solidity_style), Ok(()));

    // And a genuinely different tree is still refused.
    solidity_style.branch[0] = [0xff; 32];
    assert!(verify_message_batch(snapshot, &ids, &solidity_style).is_err());
}
