//! Reading EVM state: accounts, storage, and the Hyperlane tree kept in them.
//!
//! Shared by every EVM origin: Ethereum, Arbitrum, Base and Eden. Given a state root the
//! enclave already verified, establish what an account and its storage hold. The RPC supplies
//! the value and the proof, and only the check is believed.

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_rlp::Encodable;
use alloy_trie::proof::verify_proof;
use alloy_trie::{Nibbles, TrieAccount};
use hyperlane_types::{build_tree_from_slots, MerkleTreeSlots, TREE_DEPTH};

use crate::origin::{self, Tree};

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

/// A `MerkleTreeHook` account and the 33 storage slots holding its tree: 32 branch nodes,
/// then the count.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TreeProof {
    pub merkle_tree_hook: Address,
    pub account: ClaimedAccount,
    pub account_proof: Vec<Bytes>,
    pub storage_proof: Vec<ClaimedSlot>,
}

/// Prove a tree out of EVM state, for a chain whose hook keeps it from `base_slot`.
///
/// `base_slot` is always the chain's own pinned constant, never a request field: the canonical
/// Sepolia hook uses 103 and the other deployments 151, and whoever names the slot names what
/// gets read.
pub fn read_tree(proof: serde_json::Value, root: B256, base_slot: u64) -> anyhow::Result<Tree> {
    let proof: TreeProof = origin::parse("evm tree proof", proof)?;
    let storage_root = verify_account_proof(
        root,
        proof.merkle_tree_hook,
        &proof.account,
        &proof.account_proof,
    )?;
    let keys = MerkleTreeSlots::new(base_slot).storage_keys();
    anyhow::ensure!(
        proof.storage_proof.len() == keys.len(),
        "expected {} storage proofs, got {}",
        keys.len(),
        proof.storage_proof.len()
    );
    let mut values = [[0u8; 32]; TREE_DEPTH + 1];
    for (i, claimed) in proof.storage_proof.iter().enumerate() {
        anyhow::ensure!(
            claimed.slot == B256::from(keys[i]),
            "storage proof {i} is for slot {}, not the tree's",
            claimed.slot
        );
        verify_storage_proof(storage_root, claimed.slot, claimed.value, &claimed.proof)?;
        values[i] = claimed.value.to_be_bytes();
    }
    Ok(Tree {
        address: padded(proof.merkle_tree_hook),
        tree: build_tree_from_slots(&values).map_err(|e| anyhow::anyhow!("{e}"))?,
    })
}

/// Prove an account against a state root and return its storage root.
pub fn verify_account_proof(
    state_root: B256,
    address: Address,
    claimed: &ClaimedAccount,
    proof: &[Bytes],
) -> anyhow::Result<B256> {
    let account = TrieAccount {
        nonce: claimed.nonce,
        balance: claimed.balance,
        storage_root: claimed.storage_root,
        code_hash: claimed.code_hash,
    };
    let mut encoded = Vec::new();
    account.encode(&mut encoded);
    verify_proof(
        state_root,
        Nibbles::unpack(keccak256(address)),
        Some(encoded),
        proof,
    )
    .map_err(|_| {
        anyhow::anyhow!("account {address} does not prove against state root {state_root}")
    })?;
    Ok(claimed.storage_root)
}

/// Prove one storage slot against a storage root. A zero value is proven by exclusion,
/// because Ethereum does not store zeros; getting that backwards would make every unset
/// branch slot of the tree unverifiable.
pub fn verify_storage_proof(
    storage_root: B256,
    slot: B256,
    value: U256,
    proof: &[Bytes],
) -> anyhow::Result<()> {
    let expected = (!value.is_zero()).then(|| {
        let mut buf = Vec::new();
        value.encode(&mut buf);
        buf
    });
    verify_proof(
        storage_root,
        Nibbles::unpack(keccak256(slot)),
        expected,
        proof,
    )
    .map_err(|_| anyhow::anyhow!("slot {slot} does not prove against storage root {storage_root}"))
}

/// A 20-byte address as a Hyperlane message addresses it, left-padded to 32 bytes.
/// What an execution block header says about its block.
pub struct BlockHeader {
    pub state_root: B256,
    pub number: u64,
    pub timestamp: u64,
}

impl BlockHeader {
    /// A block header is an RLP list: state root is item 3, number item 8, timestamp item 11.
    /// Later forks only append items, so these positions hold for every header since London.
    pub fn decode(rlp: &[u8]) -> anyhow::Result<Self> {
        let malformed = || anyhow::anyhow!("block header RLP is malformed");
        let mut slice = rlp;
        let alloy_rlp::PayloadView::List(items) =
            alloy_rlp::Header::decode_raw(&mut slice).map_err(|_| malformed())?
        else {
            return Err(malformed());
        };
        anyhow::ensure!(slice.is_empty() && items.len() >= 12, malformed());
        let field = |i: usize| {
            let mut item = items[i];
            alloy_rlp::Header::decode_bytes(&mut item, false).map_err(|_| malformed())
        };
        let uint = |i: usize| -> anyhow::Result<u64> {
            let bytes = field(i)?;
            anyhow::ensure!(bytes.len() <= 8, malformed());
            let mut out = [0u8; 8];
            out[8 - bytes.len()..].copy_from_slice(bytes);
            Ok(u64::from_be_bytes(out))
        };
        let root = field(3)?;
        anyhow::ensure!(root.len() == 32, malformed());
        Ok(Self {
            state_root: B256::from_slice(root),
            number: uint(8)?,
            timestamp: uint(11)?,
        })
    }
}

pub fn padded(address: Address) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(address.as_slice());
    out
}

/// `eth_getProof` reports integers as hex quantities; accept those or plain numbers, so
/// fixtures can be verbatim RPC responses.
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
