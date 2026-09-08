//! What the enclave refuses before it verifies anything.
//!
//! Two criticals lived in `build_attested_update`, and both were the same mistake: taking
//! "where to look" from whoever was asking, then attesting something else. Proving a tree, or
//! a rollup contract, constrains nothing on its own - anyone can deploy either and prove it
//! honestly against the real state root. What matters is that what was read is what is
//! claimed, and that what is claimed is what the destination pins.
//!
//! These checks now run before any light-client work, so they cost nothing and need no chain
//! fixtures. Reaching past them would.

use tee_node::attest::{tree_address_of, TreeInput};
use tee_node::hyperlane_state::merkle_tree_base_slot;

const SEPOLIA_HOOK: &str = "4917a9746a7b6e0a57159ccb7f5a6744247f2d0d";
const ATTACKER_HOOK: &str = "000000000000000000000000000000000000dead";

fn evm_tree(hook: &str) -> TreeInput {
    serde_json::from_value(serde_json::json!({
        "kind": "evm",
        "merkle_tree_hook": format!("0x{hook}"),
        "account": {
            "nonce": "0x0",
            "balance": "0x0",
            "storage_root": format!("0x{}", "00".repeat(32)),
            "code_hash": format!("0x{}", "00".repeat(32)),
        },
        "account_proof": [],
        "storage_proof": [],
    }))
    .expect("tree input must deserialise")
}

fn padded(hook: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(&hex::decode(hook).unwrap());
    out
}

/// The merkle-address hole: read one hook, attest another. `build_attested_update` compares
/// these two, and the destination cannot - it only knows the value it pinned, which is
/// exactly the value an attacker would claim.
#[test]
fn a_tree_proven_elsewhere_does_not_match_the_canonical_address() {
    assert_eq!(tree_address_of(&evm_tree(SEPOLIA_HOOK)), padded(SEPOLIA_HOOK));
    assert_ne!(tree_address_of(&evm_tree(ATTACKER_HOOK)), padded(SEPOLIA_HOOK));
}

/// Hyperlane addresses are 32 bytes and an EVM one is left-padded, so the comparison has to
/// be done in that form rather than on the raw twenty.
#[test]
fn an_evm_hook_is_compared_left_padded() {
    let padded_hook = tree_address_of(&evm_tree(SEPOLIA_HOOK));
    assert_eq!(&padded_hook[..12], &[0u8; 12]);
    assert_eq!(hex::encode(&padded_hook[12..]), SEPOLIA_HOOK);
}

#[test]
fn a_celestia_hook_id_is_used_as_given() {
    let mut id = [0u8; 32];
    id[..20].copy_from_slice(b"router_post_dispatch");
    let tree = TreeInput::Celestia { hook_id: id, hook_bytes: Vec::new(), proof: Default::default() };
    assert_eq!(tree_address_of(&tree), id);
}

/// Several fields have been taken out of the request because the caller should never have
/// chosen them: the tree's base slot, the L2 anchor and its layout, the clock. A stale caller
/// still sending those would have them silently ignored, which looks like working.
///
/// `deny_unknown_fields` does not help inside `tree` or `origin`: serde ignores it on
/// internally tagged enums, so it would read as a guard and be none. This test exists to
/// record that, and to pin the version check that does the job instead.
#[test]
fn a_retired_field_inside_the_tree_is_silently_ignored() {
    let mut json = serde_json::json!({
        "kind": "evm",
        "merkle_tree_hook": format!("0x{SEPOLIA_HOOK}"),
        "account": {
            "nonce": "0x0", "balance": "0x0",
            "storage_root": format!("0x{}", "00".repeat(32)),
            "code_hash": format!("0x{}", "00".repeat(32)),
        },
        "account_proof": [],
        "storage_proof": [],
    });
    assert!(serde_json::from_value::<TreeInput>(json.clone()).is_ok());

    json["base_slot"] = serde_json::json!(151);
    assert!(
        serde_json::from_value::<TreeInput>(json).is_ok(),
        "serde cannot deny unknown fields on a tagged enum; the version check is the guard"
    );
}

/// What actually stops a stale caller: the enclave refuses a request that does not name the
/// protocol it speaks, so drift is a refusal rather than a value quietly dropped.
#[test]
fn a_request_without_the_current_protocol_is_refused() {
    use tee_node::attest::{AttestRequest, PROTOCOL_VERSION};

    let missing = serde_json::json!({ "trusted_state": "00" });
    assert!(
        serde_json::from_value::<AttestRequest>(missing).is_err(),
        "a request with no protocol field must not parse"
    );
    assert_eq!(PROTOCOL_VERSION, 3, "bump this when the request shape changes");
}

/// Each origin's tree layout is fixed, and an unknown origin gets no default: guessing would
/// mean reading whichever slots happened to line up.
#[test]
fn the_tree_layout_is_pinned_per_origin() {
    assert_eq!(merkle_tree_base_slot(11155111), Some(103), "Sepolia's canonical hook");
    assert_eq!(merkle_tree_base_slot(421614), Some(151), "Arbitrum Sepolia");
    assert_eq!(merkle_tree_base_slot(84532), Some(151), "Base Sepolia");
    assert_eq!(merkle_tree_base_slot(999), None);
}

/// A batch that authorises nothing still consumes the destination's one-batch-per-root slot,
/// and the root must change on every update, so the slot never reopens for that root. Posted
/// faster than the relayer, that freezes the bridge with user funds locked. The replay cannot
/// catch it: with the head's own tree as the snapshot it is the identity function.
#[test]
fn an_empty_batch_is_refused() {
    use hyperlane_types::MerkleTree;
    use tee_node::hyperlane_state::{verify_message_batch, HyperlaneStateError};

    let tree = MerkleTree { branch: [[0u8; 32]; 32], count: 0 };
    let err = verify_message_batch(tree, &[], &tree).unwrap_err();
    assert!(matches!(err, HyperlaneStateError::EmptyBatch), "got {err}");
}

/// A batch has to be exactly the leaves added since the snapshot it was given.
#[test]
fn a_batch_must_reproduce_the_onchain_tree() {
    use hyperlane_types::{insert_leaf, MerkleTree};
    use tee_node::hyperlane_state::{verify_message_batch, HyperlaneStateError};

    let snapshot = MerkleTree { branch: [[0u8; 32]; 32], count: 0 };
    let mut onchain = snapshot;
    insert_leaf(&mut onchain, [1u8; 32]).unwrap();
    insert_leaf(&mut onchain, [2u8; 32]).unwrap();

    assert!(verify_message_batch(snapshot, &[[1u8; 32], [2u8; 32]], &onchain).is_ok());

    let partial = verify_message_batch(snapshot, &[[1u8; 32]], &onchain).unwrap_err();
    assert!(matches!(partial, HyperlaneStateError::CountMismatch { .. }), "got {partial}");
}

/// The span check does not pin where a batch starts, and reading it as though it did was the
/// hole that outlived the empty-batch fix.
///
/// A Hyperlane tree is incremental and every leaf that ever entered it is public, so anyone
/// can rebuild the exact tree the origin held at any past count. Hand this function the head
/// minus one leaf plus the single id that closes the gap and it passes - correctly, because
/// that really is the distance between the two trees it was handed. What it cannot see is
/// that the ISM stands at count 5, not 8, and that three transfers in between were dropped.
/// Worse than the empty batch, because it is aimed: drop one victim, let the rest through,
/// and the bridge looks healthy while that root's one batch slot is spent and those ids are
/// never attested by any later batch either.
///
/// Nothing here can catch it, and the fix is not to try. `build_attested_update` reads the
/// snapshot out of `prev_state.state_root` rather than accepting one, so by the time the span
/// is checked its start is the ISM's own position.
#[test]
fn the_span_check_alone_does_not_pin_where_a_batch_starts() {
    use hyperlane_types::{insert_leaf, MerkleTree};
    use tee_node::hyperlane_state::verify_message_batch;

    let mut ism_stands_at = MerkleTree::default();
    for i in 0..5u8 {
        insert_leaf(&mut ism_stands_at, [i; 32]).unwrap();
    }
    let mut head = ism_stands_at;
    let mut skipped = Vec::new();
    for i in 5..8u8 {
        insert_leaf(&mut head, [i; 32]).unwrap();
        skipped.push([i; 32]);
    }
    let attacker_picks = head;
    insert_leaf(&mut head, [8u8; 32]).unwrap();

    assert!(
        verify_message_batch(attacker_picks, &[[8u8; 32]], &head).is_ok(),
        "the span check is satisfied by any real intermediate tree"
    );
    assert_eq!(skipped.len(), 3, "and those three ids are the ones nobody ever attests");

    // The honest span from where the ISM actually stands carries all four.
    let honest = [[5u8; 32], [6u8; 32], [7u8; 32], [8u8; 32]];
    assert!(verify_message_batch(ism_stands_at, &honest, &head).is_ok());
}
