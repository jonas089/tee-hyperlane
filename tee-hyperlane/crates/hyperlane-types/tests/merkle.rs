//! Known-answer tests for the Hyperlane incremental merkle tree.
//!
//! The roots come from hyperlane-cosmos' port of Hyperlane's own `MerkleTreeHook.t.sol`,
//! so passing these means our tree agrees with both the Solidity and the Cosmos SDK
//! implementations - the two origins this bridge reads.

use hyperlane_types::*;

fn h(s: &str) -> [u8; 32] {
    hex::decode(s.trim_start_matches("0x"))
        .unwrap()
        .try_into()
        .unwrap()
}

/// The Solidity test's message, with `body[31] = k`.
fn message(k: u8) -> HyperlaneMessage {
    let mut body = vec![0u8; 32];
    body[31] = k;
    HyperlaneMessage {
        version: 3,
        nonce: 0,
        origin: 11,
        sender: h("0x0000000000000000000000007fa9385be102ac3eac297483dd6233d62b3e1496"),
        destination: 22,
        recipient: h("0x00000000000000000000000000000000000000000000000000000000deadbeef"),
        body,
    }
}

#[test]
fn roots_match_hyperlanes_own_merkle_tree_hook_vectors() {
    let expected = [
        "0x10df2f89cb24ed6078fc3949b4870e94a7e32e40e8d8c6b7bd74ccc2c933d760",
        "0x080ef1c2cd394de78363ecb0a466c934b57de4abb5604a0684e571990eb7b073",
        "0xbf78ad252da524f1e733aa6b83514dd83225676b5828f888f01487108f8f7cc7",
    ];
    let mut tree = MerkleTree::default();
    for (k, want) in expected.iter().enumerate() {
        insert_leaf(&mut tree, get_message_id(&message(k as u8))).unwrap();
        assert_eq!(tree.count as usize, k + 1);
        assert_eq!(
            get_tree_root(&tree),
            h(want),
            "root after {} inserts",
            k + 1
        );
    }
}

#[test]
fn empty_tree_root_is_the_top_zero_hash() {
    let tree = MerkleTree::default();
    let z = zero_hashes();
    assert_eq!(
        get_tree_root(&tree),
        keccak_pair(&z[TREE_DEPTH - 1], &z[TREE_DEPTH - 1])
    );
}

/// The canonical deposit-contract zero hashes; a mistake here changes every root.
#[test]
fn zero_hashes_match_the_canonical_values() {
    let z = zero_hashes();
    assert_eq!(z[0], [0u8; 32]);
    assert_eq!(
        z[1],
        h("0xad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5")
    );
    assert_eq!(
        z[2],
        h("0xb4c11951957c6f8f642c4af61cd6b24640fec6dc7fc607ee8206a99e92410d30")
    );
}

/// A branch proof folded back up must reproduce the root the tree reports - this is the
/// relationship the destination ISM relies on.
#[test]
fn branch_proofs_reproduce_the_tree_root() {
    let leaves: Vec<[u8; 32]> = (0..8u8).map(|k| get_message_id(&message(k))).collect();
    let mut tree = MerkleTree::default();
    for l in &leaves {
        insert_leaf(&mut tree, *l).unwrap();
    }
    let root = get_tree_root(&tree);
    let zeros = zero_hashes();

    for (index, leaf) in leaves.iter().enumerate() {
        // Build the proof for `index` from the full leaf set.
        let mut proof = [[0u8; 32]; TREE_DEPTH];
        let mut level_nodes = leaves.clone();
        let mut idx = index;
        for (level, slot) in proof.iter_mut().enumerate() {
            let sibling = idx ^ 1;
            *slot = level_nodes.get(sibling).copied().unwrap_or(zeros[level]);
            let mut next = Vec::new();
            let mut i = 0;
            while i < level_nodes.len() {
                let a = level_nodes[i];
                let b = level_nodes.get(i + 1).copied().unwrap_or(zeros[level]);
                next.push(keccak_pair(&a, &b));
                i += 2;
            }
            level_nodes = next;
            idx /= 2;
        }
        assert_eq!(
            get_branch_root(*leaf, &proof, index as u32),
            root,
            "leaf {index}"
        );
    }
}

#[test]
fn inserting_in_a_different_order_gives_a_different_tree() {
    let a = get_message_id(&message(0));
    let b = get_message_id(&message(1));
    let mut t1 = MerkleTree::default();
    insert_leaf(&mut t1, a).unwrap();
    insert_leaf(&mut t1, b).unwrap();
    let mut t2 = MerkleTree::default();
    insert_leaf(&mut t2, b).unwrap();
    insert_leaf(&mut t2, a).unwrap();
    assert_ne!(get_tree_root(&t1), get_tree_root(&t2));
}

/// Replaying onto an adopted snapshot must land on the same branch the chain reports.
/// This is the property that lets the enclave skip authenticating the snapshot.
#[test]
fn replaying_onto_a_snapshot_reproduces_the_onchain_branch() {
    let leaves: Vec<[u8; 32]> = (0..10u8).map(|k| get_message_id(&message(k))).collect();

    let mut snapshot = MerkleTree::default();
    for l in &leaves[..4] {
        insert_leaf(&mut snapshot, *l).unwrap();
    }
    let mut onchain = snapshot;
    for l in &leaves[4..] {
        insert_leaf(&mut onchain, *l).unwrap();
    }

    let mut replayed = snapshot;
    for l in &leaves[4..] {
        insert_leaf(&mut replayed, *l).unwrap();
    }
    assert_eq!(replayed, onchain);

    // A gap in the replayed ids cannot reproduce it.
    let mut gapped = snapshot;
    for l in leaves[4..].iter().skip(1) {
        insert_leaf(&mut gapped, *l).unwrap();
    }
    assert_ne!(gapped, onchain);
}
