//! What the enclave refuses, whatever the origin.
//!
//! Every origin goes through the same pipeline in `attest::attest_with`, so its checks are
//! tested once here against a stand-in chain, with no light-client fixtures. Each check exists
//! because its absence was once a critical: taking "where to look" from whoever was asking,
//! then attesting something else.

use std::sync::Mutex;

use alloy_primitives::B256;
use hyperlane_types::{insert_leaf, MerkleTree};
use serde_json::{json, Value};
use tee_node::attest::{attest_with, AttestRequest, PROTOCOL_VERSION};
use tee_node::origin::{Chain, Head, Origin, Tree};
use tee_node::state::IsmState;

const HOOK: [u8; 32] = [7u8; 32];
const TRUSTED_ROOT: [u8; 32] = [1u8; 32];
const NEW_ROOT: [u8; 32] = [2u8; 32];
const DOMAIN: u32 = 4242;

/// A chain whose head is always `NEW_ROOT` and whose tree has one leaf more there than under
/// `TRUSTED_ROOT`. Records every root it was asked to read a tree under.
struct StandIn {
    tree_at: [u8; 32],
    asked: Mutex<Vec<B256>>,
}

fn snapshot_tree() -> MerkleTree {
    let mut t = MerkleTree::default();
    insert_leaf(&mut t, [10u8; 32]).unwrap();
    t
}

fn head_tree() -> MerkleTree {
    let mut t = snapshot_tree();
    insert_leaf(&mut t, [11u8; 32]).unwrap();
    t
}

impl Origin for StandIn {
    fn verify(&self, _input: Value, _trusted: &IsmState) -> anyhow::Result<Head> {
        Ok(Head {
            root: NEW_ROOT.into(),
            height: 20,
            timestamp: 2000,
            store_commit: [9u8; 32],
            attested_at: 2100,
        })
    }

    fn merkle_tree(&self, _proof: Value, root: B256) -> anyhow::Result<Tree> {
        self.asked.lock().unwrap().push(root);
        let tree = if root.0 == TRUSTED_ROOT {
            snapshot_tree()
        } else if root.0 == NEW_ROOT {
            head_tree()
        } else {
            anyhow::bail!("no tree under {root}")
        };
        Ok(Tree {
            address: self.tree_at,
            tree,
        })
    }
}

fn chain(tree_at: [u8; 32]) -> Chain {
    on(Box::leak(Box::new(StandIn {
        tree_at,
        asked: Mutex::new(Vec::new()),
    })))
}

fn on(origin: &'static StandIn) -> Chain {
    Chain {
        name: "stand-in",
        domain: DOMAIN,
        origin,
    }
}

fn trusted() -> IsmState {
    IsmState {
        state_root: TRUSTED_ROOT,
        origin_domain: DOMAIN,
        height: 10,
        timestamp: 1000,
        lc_store_commit: [3u8; 32],
        identity_digest: [4u8; 32],
    }
}

fn request(trusted: IsmState, ids: Vec<[u8; 32]>) -> AttestRequest {
    serde_json::from_value(json!({
        "protocol": PROTOCOL_VERSION,
        "trusted_state": hex::encode(trusted.encode()),
        "chain": "stand-in",
        "input": {},
        "tree": {},
        "tree_snapshot": {},
        "message_ids": ids,
        "merkle_tree_address": HOOK,
    }))
    .unwrap()
}

#[test]
fn a_valid_step_moves_the_state_to_the_verified_head() {
    let (update, _) = attest_with(&chain(HOOK), request(trusted(), vec![[11u8; 32]])).unwrap();
    let next = update.new_state;
    assert_eq!(next.state_root, NEW_ROOT);
    assert_eq!((next.height, next.timestamp), (20, 2000));
    assert_eq!(next.lc_store_commit, [9u8; 32]);
    assert_eq!(next.origin_domain, DOMAIN);
    assert_eq!(
        next.identity_digest, [4u8; 32],
        "the identity is carried, never chosen"
    );
    assert_eq!(update.attested_at, 2100);
    assert_eq!(update.prev_state, trusted());
}

/// The merkle-address hole: read one hook, attest another. The destination cannot catch it -
/// it only knows the address it pinned, which is exactly what an attacker would claim.
#[test]
fn a_tree_proven_at_another_hook_is_refused() {
    let err = attest_with(&chain([0xde; 32]), request(trusted(), vec![[11u8; 32]])).unwrap_err();
    assert!(err.to_string().contains("was attested"), "{err}");
}

/// One origin's attestation must not advance another origin's ISM.
#[test]
fn an_ism_for_another_domain_is_refused() {
    let mut other = trusted();
    other.origin_domain = DOMAIN + 1;
    let err = attest_with(&chain(HOOK), request(other, vec![[11u8; 32]])).unwrap_err();
    assert!(err.to_string().contains("domain"), "{err}");
}

/// Both ends of the replay come from state the ISM trusts. If the snapshot were read under
/// anything else, a caller could hand in any earlier tree and skip the messages in between.
#[test]
fn the_snapshot_is_read_under_the_trusted_root() {
    static RECORDING: StandIn = StandIn {
        tree_at: HOOK,
        asked: Mutex::new(Vec::new()),
    };
    attest_with(&on(&RECORDING), request(trusted(), vec![[11u8; 32]])).unwrap();
    let asked = RECORDING.asked.lock().unwrap().clone();
    assert_eq!(asked, vec![B256::from(TRUSTED_ROOT), B256::from(NEW_ROOT)]);
}

#[test]
fn a_batch_that_does_not_close_the_gap_is_refused() {
    assert!(attest_with(&chain(HOOK), request(trusted(), vec![])).is_err());
    assert!(attest_with(&chain(HOOK), request(trusted(), vec![[12u8; 32]])).is_err());
}

/// What stops a stale caller: a request that does not speak this protocol is refused rather
/// than having fields silently dropped.
#[test]
fn a_request_for_another_protocol_is_refused() {
    let mut req = request(trusted(), vec![[11u8; 32]]);
    req.protocol = PROTOCOL_VERSION + 1;
    assert!(attest_with(&chain(HOOK), req).is_err());
    assert!(serde_json::from_value::<AttestRequest>(json!({ "trusted_state": "00" })).is_err());
    assert_eq!(
        PROTOCOL_VERSION, 1,
        "bump this when the request shape changes"
    );
}

#[test]
fn an_unknown_chain_is_refused() {
    let mut req = request(trusted(), vec![[11u8; 32]]);
    req.chain = "nowhere".into();
    assert!(tee_node::attest::build_attested_update(req).is_err());
}

/// Names are how requests and configs find a chain, domains are what ISMs pin. A duplicate of
/// either would make one of two chains unreachable, or reachable under the other's name.
#[test]
fn every_chain_has_a_unique_name_and_domain() {
    let all = tee_node::origin::chains();
    let mut names: Vec<_> = all.iter().map(|c| c.name).collect();
    let mut domains: Vec<_> = all.iter().map(|c| c.domain).collect();
    names.sort();
    domains.sort();
    names.dedup();
    domains.dedup();
    assert_eq!(names.len(), all.len());
    assert_eq!(domains.len(), all.len());
    assert_eq!(names, ["arbitrum", "base", "celestia", "eden", "ethereum"]);
}

/// A batch that authorises nothing still consumes the destination's one-batch-per-root slot,
/// and the root must change on every update, so the slot never reopens for that root. Posted
/// faster than the relayer, that freezes the bridge with user funds locked. The replay cannot
/// catch it: with the head's own tree as the snapshot it is the identity function.
#[test]
fn an_empty_batch_is_refused() {
    use hyperlane_types::MerkleTree;
    use tee_node::attest::{verify_message_batch, BatchError};

    let tree = MerkleTree {
        branch: [[0u8; 32]; 32],
        count: 0,
    };
    let err = verify_message_batch(tree, &[], &tree).unwrap_err();
    assert!(matches!(err, BatchError::EmptyBatch), "got {err}");
}

/// A batch has to be exactly the leaves added since the snapshot it was given.
#[test]
fn a_batch_must_reproduce_the_onchain_tree() {
    use hyperlane_types::{insert_leaf, MerkleTree};
    use tee_node::attest::{verify_message_batch, BatchError};

    let snapshot = MerkleTree {
        branch: [[0u8; 32]; 32],
        count: 0,
    };
    let mut onchain = snapshot;
    insert_leaf(&mut onchain, [1u8; 32]).unwrap();
    insert_leaf(&mut onchain, [2u8; 32]).unwrap();

    assert!(verify_message_batch(snapshot, &[[1u8; 32], [2u8; 32]], &onchain).is_ok());

    let partial = verify_message_batch(snapshot, &[[1u8; 32]], &onchain).unwrap_err();
    assert!(
        matches!(partial, BatchError::CountMismatch { .. }),
        "got {partial}"
    );
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
/// Nothing here can catch it, and the fix is not to try. `attest_with` reads the snapshot
/// under `trusted_state.state_root` rather than accepting one, so by the time the span is
/// checked its start is the ISM's own position. `the_snapshot_is_read_under_the_trusted_root`
/// pins that.
#[test]
fn the_span_check_alone_does_not_pin_where_a_batch_starts() {
    use hyperlane_types::{insert_leaf, MerkleTree};
    use tee_node::attest::verify_message_batch;

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
    assert_eq!(
        skipped.len(),
        3,
        "and those three ids are the ones nobody ever attests"
    );

    // The honest span from where the ISM actually stands carries all four.
    let honest = [[5u8; 32], [6u8; 32], [7u8; 32], [8u8; 32]];
    assert!(verify_message_batch(ism_stands_at, &honest, &head).is_ok());
}
