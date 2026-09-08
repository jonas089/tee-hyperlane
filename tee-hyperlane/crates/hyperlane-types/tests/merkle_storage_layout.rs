//! Confirms the EVM storage layout against Hyperlane's live Sepolia merkleTreeHook.
//!
//! The fixture was captured with eth_getProof at a pinned block. Reconstructing `root()`
//! from raw storage is what proves both the slot layout and our tree implementation agree
//! with the deployed Solidity - and it is what caught that celestia-zkevm's slot-151
//! constant does not apply to this deployment.

use hyperlane_types::*;

#[derive(serde::Deserialize)]
struct Fixture {
    merkle_tree_hook: String,
    block: String,
    branch: Vec<String>,
    count: u32,
    root: String,
    count_slot: u64,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("../testdata/sepolia_merkle_tree_hook.json")).unwrap()
}

fn h(s: &str) -> [u8; 32] {
    hex::decode(s.trim_start_matches("0x"))
        .unwrap()
        .try_into()
        .unwrap()
}

#[test]
fn live_sepolia_storage_reconstructs_the_reported_root() {
    let f = fixture();
    let mut values = [[0u8; 32]; TREE_DEPTH + 1];
    for (i, b) in f.branch.iter().enumerate() {
        values[i] = h(b);
    }
    values[TREE_DEPTH][28..].copy_from_slice(&f.count.to_be_bytes());

    let tree = build_tree_from_slots(&values).unwrap();
    assert_eq!(tree.count, f.count);
    assert_eq!(
        get_tree_root(&tree),
        h(&f.root),
        "reconstructed root disagrees with {} root() at block {}",
        f.merkle_tree_hook,
        f.block
    );
}

#[test]
fn storage_keys_are_the_33_consecutive_slots_starting_at_the_base() {
    let slots = MerkleTreeSlots::new(SEPOLIA_MERKLE_TREE_BASE_SLOT);
    let keys = slots.storage_keys();
    assert_eq!(keys.len(), 33);
    assert_eq!(u64::from_be_bytes(keys[0][24..].try_into().unwrap()), 103);
    assert_eq!(u64::from_be_bytes(keys[32][24..].try_into().unwrap()), 135);
    assert_eq!(slots.count_slot(), fixture().count_slot);
}

#[test]
fn a_wrong_base_slot_is_caught_rather_than_silently_accepted() {
    // Reading a slot that holds something other than a small count must fail loudly.
    let mut values = [[0u8; 32]; TREE_DEPTH + 1];
    values[TREE_DEPTH] = [0xff; 32];
    assert!(matches!(
        build_tree_from_slots(&values),
        Err(LayoutError::CountNotUint32(_))
    ));
}
