//! Reading the Hyperlane tree out of Celestia state, against a live mocha-5 app hash.
//!
//! The fixture is a real `abci_query ... prove=true` response for the merkle tree hook this
//! bridge deployed, plus the app hash from the *next* block's header, which is the header the
//! light client verifies. Everything goes through `CELESTIA.origin.merkle_tree`, the same call
//! the enclave makes.

use base64::{engine::general_purpose::STANDARD, Engine};
use hyperlane_types::get_tree_root;
use serde_json::{json, Value};
use tee_node::chains::l1::celestia::CELESTIA;

#[derive(serde::Deserialize)]
struct Fixture {
    chain_id: String,
    height: u64,
    app_hash_height: u64,
    app_hash: String,
    value_b64: String,
    proof_ops: Vec<Step>,
}

#[derive(serde::Deserialize)]
struct Step {
    #[serde(rename = "type")]
    proof_type: String,
    key_b64: String,
    data_b64: String,
}

/// The hook this bridge deployed: post-dispatch submodule 2, collection 4, internal id 0.
const HOOK_ID: [u8; 32] = {
    let mut id = [0u8; 32];
    id[0] = 0x72;
    id
};

fn fixture() -> (Fixture, [u8; 32]) {
    let f: Fixture =
        serde_json::from_str(include_str!("../testdata/mocha_merkle_tree_hook.json")).unwrap();
    let app_hash = hex::decode(&f.app_hash).unwrap().try_into().unwrap();
    (f, app_hash)
}

fn proof(f: &Fixture, hook_id: [u8; 32]) -> Value {
    let steps: Vec<Value> = f
        .proof_ops
        .iter()
        .map(|s| {
            json!({
                "proof_type": s.proof_type,
                "key": STANDARD.decode(&s.key_b64).unwrap(),
                "data": STANDARD.decode(&s.data_b64).unwrap(),
            })
        })
        .collect();
    json!({
        "hook_id": hook_id,
        "hook_bytes": STANDARD.decode(&f.value_b64).unwrap(),
        "steps": steps,
    })
}

/// app hash -> store step -> iavl step -> MerkleTreeHook protobuf -> tree root, compared with
/// the root hyperlane-cosmos itself reports. Passing it also pins the storage key the enclave
/// derives from the hook id, since the iavl step is for exactly that key.
#[test]
fn a_live_merkle_tree_hook_proves_and_decodes_to_the_reported_root() {
    let (f, app_hash) = fixture();
    assert_eq!(f.chain_id, "mocha-5");
    assert_eq!(
        f.app_hash_height,
        f.height + 1,
        "app hash lags state by one block"
    );

    let read = CELESTIA
        .origin
        .merkle_tree(proof(&f, HOOK_ID), app_hash.into())
        .unwrap();
    assert_eq!(read.address, HOOK_ID);
    assert_eq!(read.tree.count, 3, "three messages dispatched");
    assert_eq!(
        hex::encode(get_tree_root(&read.tree)),
        "f6d1f4ac533facc111e4fea4a757d7a790fb16f6d4ed4e225ac6cb29d119e09b"
    );
}

fn refused(p: Value, app_hash: [u8; 32]) {
    assert!(CELESTIA.origin.merkle_tree(p, app_hash.into()).is_err());
}

#[test]
fn a_tampered_hook_is_refused() {
    let (f, app_hash) = fixture();
    let mut p = proof(&f, HOOK_ID);
    let bytes = p["hook_bytes"].as_array_mut().unwrap();
    let last = bytes.len() - 1;
    bytes[last] = json!(bytes[last].as_u64().unwrap() ^ 0xff);
    refused(p, app_hash);
}

#[test]
fn a_wrong_app_hash_is_refused() {
    let (f, _) = fixture();
    refused(proof(&f, HOOK_ID), [0xab; 32]);
}

/// A real proof for this hook must not be accepted as a proof for a different hook.
#[test]
fn a_proof_for_another_hook_is_refused() {
    let (f, app_hash) = fixture();
    let mut other = HOOK_ID;
    other[31] = 1;
    refused(proof(&f, other), app_hash);
}

#[test]
fn the_steps_must_be_both_present_and_in_order() {
    let (f, app_hash) = fixture();
    let mut swapped = proof(&f, HOOK_ID);
    swapped["steps"].as_array_mut().unwrap().swap(0, 1);
    refused(swapped, app_hash);

    let mut one = proof(&f, HOOK_ID);
    one["steps"].as_array_mut().unwrap().pop();
    refused(one, app_hash);
}
