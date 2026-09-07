//! Proving a value out of a chain's state, without trusting whoever served the proof.
//!
//! Two chains, two commitment schemes, one job: given a state root the light client has
//! already verified, establish what a particular key holds.
//!
//! * Ethereum and its L2s use a Merkle-Patricia trie, keyed by `keccak(address)` then
//!   `keccak(slot)`.
//! * Celestia commits application state as an IAVL tree per module store under a simple
//!   merkle tree of stores, so one key needs two chained ics23 proofs.
//!
//! In both cases the RPC supplies the claimed value *and* the proof, and we only ever check
//! the claim - a wrong or malicious response fails to verify rather than yielding a wrong
//! answer.

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_rlp::Encodable;
use alloy_trie::proof::verify_proof;
use alloy_trie::{Nibbles, TrieAccount};

/// An account as `eth_getProof` reports it. Untrusted until proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClaimedAccount {
    #[serde(with = "hex_quantity")]
    pub nonce: u64,
    pub balance: U256,
    pub storage_root: B256,
    pub code_hash: B256,
}

/// One storage slot as `eth_getProof` reports it. Untrusted until proven.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClaimedSlot {
    pub slot: B256,
    pub value: U256,
    pub proof: Vec<Bytes>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum MptError {
    #[error("account {address} does not prove against state root {root}")]
    AccountProof { address: Address, root: B256 },
    #[error("storage slot {slot} does not prove against storage root {root}")]
    StorageProof { slot: B256, root: B256 },
}

/// Prove a contract's account against the state root and return its storage root.
pub fn verify_account_proof(
    state_root: B256,
    address: Address,
    claimed: &ClaimedAccount,
    proof: &[Bytes],
) -> Result<B256, MptError> {
    let account = TrieAccount {
        nonce: claimed.nonce,
        balance: claimed.balance,
        storage_root: claimed.storage_root,
        code_hash: claimed.code_hash,
    };
    let mut encoded = Vec::new();
    account.encode(&mut encoded);

    let key = Nibbles::unpack(keccak256(address.as_slice()));
    verify_proof(state_root, key, Some(encoded), proof)
        .map_err(|_| MptError::AccountProof { address, root: state_root })?;
    Ok(claimed.storage_root)
}

/// Prove one storage slot against a storage root.
///
/// A zero value is proven by an *exclusion* proof, because Ethereum does not store zeros.
/// Getting this backwards would make every unset merkle-tree branch slot unverifiable.
pub fn verify_storage_proof(
    storage_root: B256,
    slot: B256,
    value: U256,
    proof: &[Bytes],
) -> Result<(), MptError> {
    let expected = if value.is_zero() {
        None
    } else {
        let mut buf = Vec::new();
        value.encode(&mut buf);
        Some(buf)
    };
    let key = Nibbles::unpack(keccak256(slot.as_slice()));
    verify_proof(storage_root, key, expected, proof)
        .map_err(|_| MptError::StorageProof { slot, root: storage_root })
}

/// Prove every slot in one go, returning the values in the order the slots were given.
pub fn verify_storage_slots(
    storage_root: B256,
    slots: &[ClaimedSlot],
) -> Result<Vec<U256>, MptError> {
    slots
        .iter()
        .map(|s| {
            verify_storage_proof(storage_root, s.slot, s.value, &s.proof)?;
            Ok(s.value)
        })
        .collect()
}

/// JSON-RPC returns integers as hex quantities ("0x1"), so accept those as well as plain
/// numbers. Keeping this here means fixtures can be verbatim RPC responses.
mod hex_quantity {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("0x{v:x}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Quantity {
            Hex(String),
            Number(u64),
        }
        match Quantity::deserialize(d)? {
            Quantity::Number(n) => Ok(n),
            Quantity::Hex(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16)
                .map_err(serde::de::Error::custom),
        }
    }
}

// ============================================================================
// Celestia: two-level ics23
// ============================================================================

use ics23::commitment_proof::Proof;
use ics23::{
    calculate_existence_root, iavl_spec, tendermint_spec, CommitmentProof, HostFunctionsManager,
};
use prost::Message;

use hyperlane_types::{MerkleTree, TREE_DEPTH};

/// hyperlane-cosmos keeps merkle tree hooks under this prefix in the `hyperlane` store:
/// post-dispatch submodule id 2, collection 4.
pub const MERKLE_TREE_HOOKS_PREFIX: [u8; 2] = [2, 4];
/// The cosmos SDK module store hyperlane-cosmos writes into.
pub const HYPERLANE_STORE: &str = "hyperlane";

/// One level of a cosmos ABCI store proof, as `proofOps` delivers it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoreProofOp {
    /// `ics23:iavl` for the module store, `ics23:simple` for the store list.
    pub proof_type: String,
    pub key: Vec<u8>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum CelestiaStateError {
    #[error("expected two proof ops (iavl then simple), got {0}")]
    WrongProofShape(usize),
    #[error("proof op {index} has type `{got}`, expected `{expected}`")]
    WrongProofType { index: usize, got: String, expected: &'static str },
    #[error("proof op {0} is not a decodable ics23 commitment proof")]
    UndecodableProof(usize),
    #[error("proof op {0} is not an existence proof")]
    NotAnExistenceProof(usize),
    #[error("the {level} proof does not verify")]
    ProofRejected { level: &'static str },
    #[error("store proof is for store `{got}`, expected `{expected}`")]
    WrongStore { got: String, expected: String },
    #[error("iavl proof is for a different key than requested")]
    WrongKey,
    #[error("merkle tree hook protobuf is malformed")]
    MalformedHook,
    #[error("merkle tree hook has no tree")]
    HookHasNoTree,
    #[error("tree branch has {0} entries, expected {TREE_DEPTH}")]
    WrongBranchLength(usize),
    #[error("tree branch entry {0} is not 32 bytes")]
    BranchEntryNotHash(usize),
}

/// The storage key hyperlane-cosmos uses for a merkle tree hook.
///
/// `collections.Map` keys are the prefix followed by the big-endian uint64 internal id,
/// which is the low 8 bytes of the hook's 32-byte HexAddress.
pub fn get_merkle_tree_hook_key(hook_id: [u8; 32]) -> Vec<u8> {
    let mut key = MERKLE_TREE_HOOKS_PREFIX.to_vec();
    key.extend_from_slice(&hook_id[24..]);
    key
}

/// Verify a value against a light-client-verified app hash, through both tree levels.
pub fn verify_store_value(
    app_hash: [u8; 32],
    store: &str,
    key: &[u8],
    value: &[u8],
    ops: &[StoreProofOp],
) -> Result<(), CelestiaStateError> {
    if ops.len() != 2 {
        return Err(CelestiaStateError::WrongProofShape(ops.len()));
    }
    for (index, expected) in [(0usize, "ics23:iavl"), (1, "ics23:simple")] {
        if ops[index].proof_type != expected {
            return Err(CelestiaStateError::WrongProofType {
                index,
                got: ops[index].proof_type.clone(),
                expected,
            });
        }
    }
    if ops[0].key != key {
        return Err(CelestiaStateError::WrongKey);
    }
    let store_name = String::from_utf8_lossy(&ops[1].key).into_owned();
    if store_name != store {
        return Err(CelestiaStateError::WrongStore {
            got: store_name,
            expected: store.to_string(),
        });
    }

    // Inner: the key exists in the module's IAVL tree, under some store root.
    let inner = decode_existence(&ops[0].data, 0)?;
    let store_root = calculate_existence_root::<HostFunctionsManager>(&inner)
        .map_err(|_| CelestiaStateError::ProofRejected { level: "iavl" })?;
    if !ics23::verify_membership::<HostFunctionsManager>(
        &wrap(inner),
        &iavl_spec(),
        &store_root,
        key,
        value,
    ) {
        return Err(CelestiaStateError::ProofRejected { level: "iavl" });
    }

    // Outer: that store root is the value the app hash commits to for this store name.
    let outer = decode_existence(&ops[1].data, 1)?;
    if !ics23::verify_membership::<HostFunctionsManager>(
        &wrap(outer),
        &tendermint_spec(),
        &app_hash.to_vec(),
        store.as_bytes(),
        &store_root,
    ) {
        return Err(CelestiaStateError::ProofRejected { level: "store" });
    }
    Ok(())
}

/// Prove the origin merkle tree hook out of Celestia state and return its tree.
/// A cosmos store proof for one merkle tree hook.
pub type Ics23TreeProof = Vec<StoreProofOp>;

pub fn get_celestia_merkle_tree(
    app_hash: [u8; 32],
    hook_id: [u8; 32],
    hook_bytes: &[u8],
    ops: &Ics23TreeProof,
) -> Result<MerkleTree, CelestiaStateError> {
    let key = get_merkle_tree_hook_key(hook_id);
    verify_store_value(app_hash, HYPERLANE_STORE, &key, hook_bytes, ops)?;
    decode_merkle_tree_hook(hook_bytes)
}

/// hyperlane-cosmos `MerkleTreeHook`; only the tree matters to us.
#[derive(Clone, PartialEq, Message)]
struct MerkleTreeHookProto {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    mailbox_id: String,
    #[prost(string, tag = "3")]
    owner: String,
    #[prost(message, optional, tag = "4")]
    tree: Option<TreeProto>,
}

/// hyperlane-cosmos `Tree`: the same incremental tree the Solidity hook keeps.
#[derive(Clone, PartialEq, Message)]
struct TreeProto {
    #[prost(bytes = "vec", repeated, tag = "1")]
    branch: Vec<Vec<u8>>,
    #[prost(uint32, tag = "2")]
    count: u32,
}

pub fn decode_merkle_tree_hook(bytes: &[u8]) -> Result<MerkleTree, CelestiaStateError> {
    let hook =
        MerkleTreeHookProto::decode(bytes).map_err(|_| CelestiaStateError::MalformedHook)?;
    let tree = hook.tree.ok_or(CelestiaStateError::HookHasNoTree)?;
    if tree.branch.len() != TREE_DEPTH {
        return Err(CelestiaStateError::WrongBranchLength(tree.branch.len()));
    }
    let mut branch = [[0u8; 32]; TREE_DEPTH];
    for (i, node) in tree.branch.iter().enumerate() {
        branch[i] = node
            .as_slice()
            .try_into()
            .map_err(|_| CelestiaStateError::BranchEntryNotHash(i))?;
    }
    Ok(MerkleTree { branch, count: tree.count })
}

fn decode_existence(
    data: &[u8],
    index: usize,
) -> Result<ics23::ExistenceProof, CelestiaStateError> {
    let proof = CommitmentProof::decode(data)
        .map_err(|_| CelestiaStateError::UndecodableProof(index))?;
    match proof.proof {
        Some(Proof::Exist(e)) => Ok(e),
        _ => Err(CelestiaStateError::NotAnExistenceProof(index)),
    }
}

fn wrap(e: ics23::ExistenceProof) -> CommitmentProof {
    CommitmentProof { proof: Some(Proof::Exist(e)) }
}
