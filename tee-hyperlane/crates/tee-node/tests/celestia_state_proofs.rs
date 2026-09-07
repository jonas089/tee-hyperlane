//! Verifies the two-level cosmos store proof against a live Celestia mocha-5 app hash.
//!
//! The fixture is a real `abci_query ... prove=true` response plus the app hash from the
//! *next* block's header, which is the header a light client actually verifies.

use base64::{engine::general_purpose::STANDARD, Engine};
use hyperlane_types::{get_tree_root, insert_leaf, MerkleTree, TREE_DEPTH};
use tee_node::state_proofs::*;

#[derive(serde::Deserialize)]
struct RawOp {
    #[serde(rename = "type")]
    proof_type: String,
    key_b64: String,
    data_b64: String,
}

#[derive(serde::Deserialize)]
struct Fixture {
    chain_id: String,
    height: u64,
    app_hash_height: u64,
    app_hash: String,
    store: String,
    key_hex: String,
    value_b64: String,
    proof_ops: Vec<RawOp>,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("../testdata/mocha_store_proof.json")).unwrap()
}

fn parts(f: &Fixture) -> ([u8; 32], Vec<u8>, Vec<u8>, Vec<StoreProofOp>) {
    let app_hash: [u8; 32] = hex::decode(&f.app_hash).unwrap().try_into().unwrap();
    let key = hex::decode(&f.key_hex).unwrap();
    let value = STANDARD.decode(&f.value_b64).unwrap();
    let ops = f
        .proof_ops
        .iter()
        .map(|o| StoreProofOp {
            proof_type: o.proof_type.clone(),
            key: STANDARD.decode(&o.key_b64).unwrap(),
            data: STANDARD.decode(&o.data_b64).unwrap(),
        })
        .collect();
    (app_hash, key, value, ops)
}

#[test]
fn a_live_mocha_store_value_verifies_against_the_app_hash() {
    let f = fixture();
    assert_eq!(f.chain_id, "mocha-5");
    assert_eq!(f.app_hash_height, f.height + 1, "app hash lags state by one block");
    let (app_hash, key, value, ops) = parts(&f);
    assert_eq!(verify_store_value(app_hash, &f.store, &key, &value, &ops), Ok(()));
}

#[test]
fn a_tampered_value_is_rejected() {
    let f = fixture();
    let (app_hash, key, mut value, ops) = parts(&f);
    value.push(b'0'); // inflate the reported supply
    assert_eq!(
        verify_store_value(app_hash, &f.store, &key, &value, &ops),
        Err(CelestiaStateError::ProofRejected { level: "iavl" })
    );
}

#[test]
fn a_wrong_app_hash_is_rejected() {
    let f = fixture();
    let (_, key, value, ops) = parts(&f);
    assert_eq!(
        verify_store_value([0xab; 32], &f.store, &key, &value, &ops),
        Err(CelestiaStateError::ProofRejected { level: "store" })
    );
}

/// A proof for one store must not be accepted as a proof for another.
#[test]
fn a_proof_for_a_different_store_is_rejected() {
    let f = fixture();
    let (app_hash, key, value, ops) = parts(&f);
    let err = verify_store_value(app_hash, "hyperlane", &key, &value, &ops).unwrap_err();
    assert!(matches!(err, CelestiaStateError::WrongStore { .. }));
}

#[test]
fn a_proof_for_a_different_key_is_rejected() {
    let f = fixture();
    let (app_hash, _, value, ops) = parts(&f);
    assert_eq!(
        verify_store_value(app_hash, &f.store, b"\x00usomethingelse", &value, &ops),
        Err(CelestiaStateError::WrongKey)
    );
}

#[test]
fn the_two_levels_must_arrive_in_order() {
    let f = fixture();
    let (app_hash, key, value, mut ops) = parts(&f);
    ops.swap(0, 1);
    let err = verify_store_value(app_hash, &f.store, &key, &value, &ops).unwrap_err();
    assert!(matches!(err, CelestiaStateError::WrongProofType { index: 0, .. }));
}

#[test]
fn a_single_level_proof_is_rejected() {
    let f = fixture();
    let (app_hash, key, value, mut ops) = parts(&f);
    ops.pop();
    assert_eq!(
        verify_store_value(app_hash, &f.store, &key, &value, &ops),
        Err(CelestiaStateError::WrongProofShape(1))
    );
}

// ---- merkle tree hook decoding ----

/// hyperlane-cosmos writes `MerkleTreeHook { id:1, mailbox_id:2, owner:3, tree:4 }` with
/// `Tree { branch:1 repeated bytes, count:2 uint32 }`.
fn encode_hook(branch: &[[u8; 32]], count: u32) -> Vec<u8> {
    fn key(field: u64, wire: u64) -> Vec<u8> {
        let mut v = Vec::new();
        let mut x = (field << 3) | wire;
        loop {
            let b = (x & 0x7f) as u8;
            x >>= 7;
            if x == 0 {
                v.push(b);
                break;
            }
            v.push(b | 0x80);
        }
        v
    }
    fn delimited(field: u64, payload: &[u8]) -> Vec<u8> {
        let mut v = key(field, 2);
        let mut len = payload.len() as u64;
        loop {
            let b = (len & 0x7f) as u8;
            len >>= 7;
            if len == 0 {
                v.push(b);
                break;
            }
            v.push(b | 0x80);
        }
        v.extend_from_slice(payload);
        v
    }
    let mut tree = Vec::new();
    for node in branch {
        tree.extend_from_slice(&delimited(1, node));
    }
    if count != 0 {
        tree.extend_from_slice(&key(2, 0));
        let mut c = count as u64;
        loop {
            let b = (c & 0x7f) as u8;
            c >>= 7;
            if c == 0 {
                tree.push(b);
                break;
            }
            tree.push(b | 0x80);
        }
    }
    let mut hook = Vec::new();
    hook.extend_from_slice(&delimited(1, b"0x726f757465725f706f73745f64697370617463680000000200000000000001"));
    hook.extend_from_slice(&delimited(2, b"0x68797065726c616e6500000000000000000000000000000000000000000000"));
    hook.extend_from_slice(&delimited(3, b"celestia1owner"));
    hook.extend_from_slice(&delimited(4, &tree));
    hook
}

#[test]
fn a_cosmos_merkle_tree_hook_decodes_to_the_same_tree_the_evm_hook_holds() {
    let mut expected = MerkleTree::default();
    for k in 0..5u8 {
        insert_leaf(&mut expected, [k; 32]).unwrap();
    }
    let bytes = encode_hook(&expected.branch, expected.count);
    let decoded = decode_merkle_tree_hook(&bytes).unwrap();
    assert_eq!(decoded, expected);
    assert_eq!(get_tree_root(&decoded), get_tree_root(&expected));
}

#[test]
fn a_hook_without_a_tree_is_rejected() {
    let bytes = encode_hook(&[], 0);
    assert_eq!(decode_merkle_tree_hook(&bytes), Err(CelestiaStateError::WrongBranchLength(0)));
}

#[test]
fn a_hook_with_a_short_branch_is_rejected() {
    let branch = [[0u8; 32]; TREE_DEPTH];
    let bytes = encode_hook(&branch[..31], 3);
    assert_eq!(decode_merkle_tree_hook(&bytes), Err(CelestiaStateError::WrongBranchLength(31)));
}

#[test]
fn hook_storage_keys_use_the_post_dispatch_prefix_and_internal_id() {
    let mut hook_id = [0u8; 32];
    hook_id[24..].copy_from_slice(&7u64.to_be_bytes());
    let key = get_merkle_tree_hook_key(hook_id);
    assert_eq!(key[..2], [2, 4], "post-dispatch submodule 2, collection 4");
    assert_eq!(u64::from_be_bytes(key[2..].try_into().unwrap()), 7);
}

/// The full Celestia read path, against the live mocha-5 deployment this bridge created.
///
/// app hash -> ics23 store proof -> ics23 iavl proof -> MerkleTreeHook protobuf ->
/// incremental tree root, compared against the root the chain itself reports. Passing this
/// means our tree agrees with hyperlane-cosmos exactly as it already agrees with the
/// Solidity hook - the two origins converge on one root.
mod live_hyperlane_hook {
    use super::*;
    use hyperlane_types::get_tree_root;

    #[derive(serde::Deserialize)]
    struct Fixture {
        chain_id: String,
        height: u64,
        app_hash_height: u64,
        app_hash: String,
        key_hex: String,
        value_b64: String,
        proof_ops: Vec<RawOp>,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../testdata/mocha_merkle_tree_hook.json")).unwrap()
    }

    /// The hook this bridge deployed: post-dispatch submodule 2, collection 4, internal id 0.
    const HOOK_ID: [u8; 32] = {
        let mut id = [0u8; 32];
        id[0] = 0x72;
        id
    };

    #[test]
    fn a_live_merkle_tree_hook_proves_and_decodes_to_the_reported_root() {
        let f = fixture();
        assert_eq!(f.chain_id, "mocha-5");
        assert_eq!(f.app_hash_height, f.height + 1);

        let app_hash: [u8; 32] = hex::decode(&f.app_hash).unwrap().try_into().unwrap();
        let key = hex::decode(&f.key_hex).unwrap();
        let value = STANDARD.decode(&f.value_b64).unwrap();
        let ops: Vec<StoreProofOp> = f
            .proof_ops
            .iter()
            .map(|o| StoreProofOp {
                proof_type: o.proof_type.clone(),
                key: STANDARD.decode(&o.key_b64).unwrap(),
                data: STANDARD.decode(&o.data_b64).unwrap(),
            })
            .collect();

        // The key the enclave derives must be the key the chain actually stores under.
        assert_eq!(get_merkle_tree_hook_key(HOOK_ID), key);

        verify_store_value(app_hash, HYPERLANE_STORE, &key, &value, &ops)
            .expect("hook must prove against the app hash");

        let tree = decode_merkle_tree_hook(&value).expect("hook protobuf");
        assert_eq!(tree.count, 3, "three messages dispatched");
        assert_eq!(
            hex::encode(get_tree_root(&tree)),
            "f6d1f4ac533facc111e4fea4a757d7a790fb16f6d4ed4e225ac6cb29d119e09b",
            "reconstructed root disagrees with the root hyperlane-cosmos reports"
        );
    }

    #[test]
    fn a_tampered_hook_value_is_rejected() {
        let f = fixture();
        let app_hash: [u8; 32] = hex::decode(&f.app_hash).unwrap().try_into().unwrap();
        let key = hex::decode(&f.key_hex).unwrap();
        let mut value = STANDARD.decode(&f.value_b64).unwrap();
        let last = value.len() - 1;
        value[last] ^= 0xff;
        let ops: Vec<StoreProofOp> = f
            .proof_ops
            .iter()
            .map(|o| StoreProofOp {
                proof_type: o.proof_type.clone(),
                key: STANDARD.decode(&o.key_b64).unwrap(),
                data: STANDARD.decode(&o.data_b64).unwrap(),
            })
            .collect();
        assert!(verify_store_value(app_hash, HYPERLANE_STORE, &key, &value, &ops).is_err());
    }
}
