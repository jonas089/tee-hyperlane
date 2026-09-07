//! Arbitrum and Base: L2 state roots derived from Ethereum, with no second light client.
//!
//! Both chains publish a commitment to their L2 state *into Ethereum L1 storage*. So once
//! the Ethereum light client has verified an L1 state root, each L2 root costs a storage
//! proof plus a keccak preimage check - not another enclave, not another consensus client.
//! They differ only in what the commitment is:
//!
//! * Arbitrum stores `confirmData = keccak(blockHash || sendRoot)` per confirmed node, so
//!   reaching the state root needs the L2 block header preimage as well.
//! * Base stores an OP Stack output root, whose preimage contains the L2 state root
//!   directly.
//!
//! Both share one caveat: only *confirmed* commitments are trustless, and confirmation waits
//! out each chain's challenge window. That latency is a property of the rollups, not of this
//! bridge.

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};

use super::AttestedRoot;
use crate::state_proofs::{verify_account_proof, verify_storage_proof, ClaimedAccount, MptError};

/// `Node.confirmData` is the third field of the struct.
const CONFIRM_DATA_FIELD_INDEX: u64 = 2;

/// Layout of the `RollupCore` storage this bridge reads. Deployment-specific, so it is
/// configuration rather than a constant - the same lesson as the merkle tree hook's base
/// slot, which differs between Hyperlane's Sepolia deployment and celestia-zkevm's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RollupLayout {
    /// Slot holding `_latestConfirmed`.
    pub latest_confirmed_slot: u64,
    /// Byte offset of `_latestConfirmed` within that slot, counted from the least
    /// significant byte.
    ///
    /// Solidity packs several uint64s into one slot, so the proven slot value is not the
    /// node number on its own. Arbitrum Sepolia's rollup packs four values into slot 117,
    /// with `_latestConfirmed` in the low 8 bytes.
    pub latest_confirmed_byte_offset: u32,
    /// Base slot of the `_nodes` mapping.
    pub nodes_mapping_slot: u64,
}

impl RollupLayout {
    /// Arbitrum Sepolia's rollup, confirmed against live L1 storage.
    pub const ARBITRUM_SEPOLIA: Self = Self {
        latest_confirmed_slot: 117,
        latest_confirmed_byte_offset: 0,
        nodes_mapping_slot: 118,
    };

    /// Pull the packed uint64 out of a proven slot value.
    pub fn read_latest_confirmed(&self, slot_value: U256) -> u64 {
        let shifted = slot_value >> (self.latest_confirmed_byte_offset * 8);
        (shifted & U256::from(u64::MAX)).to::<u64>()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArbitrumRootProof {
    pub rollup: Address,
    pub layout: RollupLayout,
    pub account: ClaimedAccount,
    pub account_proof: Vec<Bytes>,
    /// Proof of the slot holding `_latestConfirmed`.
    pub latest_confirmed_proof: Vec<Bytes>,
    /// The whole packed slot value, not just the node number.
    pub latest_confirmed_slot_value: U256,
    /// Proof of `_nodes[latest_confirmed].confirmData`.
    pub confirm_data_proof: Vec<Bytes>,
    /// Preimage of `confirmData`.
    pub l2_block_hash: B256,
    pub l2_send_root: B256,
    /// RLP of the L2 block header, whose keccak is `l2_block_hash`.
    pub l2_header_rlp: Bytes,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ArbitrumError {
    #[error(transparent)]
    Mpt(#[from] MptError),
    #[error("confirmData preimage hashes to {got}, which L1 does not store for node {node}")]
    ConfirmDataMismatch { node: u64, got: B256 },
    #[error("L2 header RLP hashes to {got}, but the node committed to {expected}")]
    HeaderHashMismatch { got: B256, expected: B256 },
    #[error("L2 header RLP is malformed")]
    MalformedHeader,
}

/// Storage slot of `_nodes[node].confirmData`: `keccak(pad32(node) || pad32(slot)) + 2`.
pub fn get_confirm_data_slot(node: u64, layout: &RollupLayout) -> B256 {
    let mut preimage = [0u8; 64];
    preimage[24..32].copy_from_slice(&node.to_be_bytes());
    preimage[56..64].copy_from_slice(&layout.nodes_mapping_slot.to_be_bytes());
    let base = U256::from_be_bytes(keccak256(preimage).0);
    B256::from(base.wrapping_add(U256::from(CONFIRM_DATA_FIELD_INDEX)))
}

/// Derive Arbitrum's L2 state root from a verified Ethereum L1 state root.
pub fn get_arbitrum_root(
    l1_state_root: B256,
    proof: &ArbitrumRootProof,
) -> Result<AttestedRoot, ArbitrumError> {
    let storage_root =
        verify_account_proof(l1_state_root, proof.rollup, &proof.account, &proof.account_proof)?;

    // Which node is confirmed is read from L1, never supplied.
    let latest_slot = B256::from(U256::from(proof.layout.latest_confirmed_slot));
    verify_storage_proof(
        storage_root,
        latest_slot,
        proof.latest_confirmed_slot_value,
        &proof.latest_confirmed_proof,
    )?;
    let node = proof.layout.read_latest_confirmed(proof.latest_confirmed_slot_value);

    let expected = keccak256([proof.l2_block_hash.as_slice(), proof.l2_send_root.as_slice()].concat());
    let slot = get_confirm_data_slot(node, &proof.layout);
    verify_storage_proof(
        storage_root,
        slot,
        U256::from_be_bytes(expected.0),
        &proof.confirm_data_proof,
    )
    .map_err(|_| ArbitrumError::ConfirmDataMismatch { node, got: expected })?;

    let header = decode_l2_header(&proof.l2_header_rlp)?;
    let got = keccak256(&proof.l2_header_rlp);
    if got != proof.l2_block_hash {
        return Err(ArbitrumError::HeaderHashMismatch { got, expected: proof.l2_block_hash });
    }

    Ok(AttestedRoot {
        state_root: header.state_root,
        height: header.number,
        timestamp: header.timestamp,
    })
}

/// The three header fields the bridge needs, by RLP position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L2Header {
    pub state_root: B256,
    pub number: u64,
    pub timestamp: u64,
}

/// Ethereum block headers are an RLP list; state root is item 3, number item 8,
/// timestamp item 11.
pub fn decode_l2_header(rlp: &[u8]) -> Result<L2Header, ArbitrumError> {
    let mut slice = rlp;
    let items = match alloy_rlp::Header::decode_raw(&mut slice)
        .map_err(|_| ArbitrumError::MalformedHeader)?
    {
        alloy_rlp::PayloadView::List(items) => items,
        _ => return Err(ArbitrumError::MalformedHeader),
    };
    if items.len() < 12 {
        return Err(ArbitrumError::MalformedHeader);
    }
    let state_root = read_hash(items[3])?;
    Ok(L2Header {
        state_root,
        number: read_uint(items[8])?,
        timestamp: read_uint(items[11])?,
    })
}

fn read_hash(item: &[u8]) -> Result<B256, ArbitrumError> {
    let mut buf = item;
    let bytes = alloy_rlp::Header::decode_bytes(&mut buf, false)
        .map_err(|_| ArbitrumError::MalformedHeader)?;
    if bytes.len() != 32 {
        return Err(ArbitrumError::MalformedHeader);
    }
    Ok(B256::from_slice(bytes))
}

fn read_uint(item: &[u8]) -> Result<u64, ArbitrumError> {
    let mut buf = item;
    let bytes = alloy_rlp::Header::decode_bytes(&mut buf, false)
        .map_err(|_| ArbitrumError::MalformedHeader)?;
    if bytes.len() > 8 {
        return Err(ArbitrumError::MalformedHeader);
    }
    let mut out = [0u8; 8];
    out[8 - bytes.len()..].copy_from_slice(bytes);
    Ok(u64::from_be_bytes(out))
}

// ============================================================================
// Base (OP Stack)
// ============================================================================

/// The preimage of an OP Stack output root.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BaseOutputRootPreimage {
    /// Always zero for output root version 1.
    pub version: B256,
    pub state_root: B256,
    pub message_passer_storage_root: B256,
    pub latest_block_hash: B256,
    pub l2_block_number: u64,
    pub l2_timestamp: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BaseRootProof {
    pub anchor_state_registry: Address,
    /// Storage slot holding the anchor's output root.
    pub anchor_root_slot: B256,
    pub account: ClaimedAccount,
    pub account_proof: Vec<Bytes>,
    pub anchor_root_proof: Vec<Bytes>,
    pub preimage: BaseOutputRootPreimage,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum BaseError {
    #[error(transparent)]
    Mpt(#[from] MptError),
    #[error("output root preimage hashes to {got}, which L1 does not store at this slot")]
    OutputRootMismatch { got: B256 },
}

pub fn hash_output_root(p: &BaseOutputRootPreimage) -> B256 {
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(p.version.as_slice());
    buf.extend_from_slice(p.state_root.as_slice());
    buf.extend_from_slice(p.message_passer_storage_root.as_slice());
    buf.extend_from_slice(p.latest_block_hash.as_slice());
    keccak256(buf)
}

/// Derive Base's L2 state root from a verified Ethereum L1 state root.
pub fn get_base_root(
    l1_state_root: B256,
    proof: &BaseRootProof,
) -> Result<AttestedRoot, BaseError> {
    let storage_root = verify_account_proof(
        l1_state_root,
        proof.anchor_state_registry,
        &proof.account,
        &proof.account_proof,
    )?;

    let output_root = hash_output_root(&proof.preimage);
    verify_storage_proof(
        storage_root,
        proof.anchor_root_slot,
        output_root.into(),
        &proof.anchor_root_proof,
    )
    .map_err(|_| BaseError::OutputRootMismatch { got: output_root })?;

    Ok(AttestedRoot {
        state_root: proof.preimage.state_root,
        height: proof.preimage.l2_block_number,
        timestamp: proof.preimage.l2_timestamp,
    })
}
