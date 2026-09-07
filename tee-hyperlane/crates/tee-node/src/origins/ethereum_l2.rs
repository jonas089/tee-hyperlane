//! Arbitrum and Base: L2 state roots derived from Ethereum, with no second light client.
//!
//! Both chains publish a commitment to their L2 state *into Ethereum L1 storage*. So once
//! the Ethereum light client has verified an L1 state root, each L2 root costs a storage
//! proof plus a keccak preimage check - not another enclave, not another consensus client.
//! They differ only in what the commitment is:
//!
//! * Arbitrum (BoLD) stores the hash of the confirmed *assertion*, whose preimage contains
//!   the L2 block hash, so reaching the state root needs the assertion preimage and then the
//!   L2 block header preimage.
//! * Base stores an OP Stack output root, whose preimage contains the L2 state root
//!   directly.
//!
//! Both share one caveat: only *confirmed* commitments are trustless, and confirmation waits
//! out each chain's challenge window. That latency is a property of the rollups, not of this
//! bridge.

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};

use super::AttestedRoot;
use crate::state_proofs::{verify_account_proof, verify_storage_proof, ClaimedAccount, MptError};

/// `AssertionNode.status`, counted in bytes from the least significant end of its slot.
/// Solidity packs `firstChildBlock`, `secondChildBlock`, `createdAtBlock` (8 bytes each) and
/// `isFirstChild` (1 byte) below it.
const ASSERTION_STATUS_BYTE_OFFSET: u32 = 25;

/// `AssertionStatus.Confirmed`. Anything else has not survived its challenge window.
const ASSERTION_CONFIRMED: u8 = 2;

/// Layout of the `RollupCore` storage this bridge reads. Deployment-specific, so it is
/// configuration rather than a constant - the same lesson as the merkle tree hook's base
/// slot, which differs between Hyperlane's Sepolia deployment and celestia-zkevm's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RollupLayout {
    /// Slot holding `_latestConfirmed`, the hash of the newest confirmed assertion.
    pub latest_confirmed_slot: u64,
    /// Base slot of the `_assertions` mapping, keyed by assertion hash.
    pub assertions_mapping_slot: u64,
}

impl RollupLayout {
    /// Arbitrum Sepolia's BoLD rollup, read off live L1 storage.
    pub const ARBITRUM_SEPOLIA: Self =
        Self { latest_confirmed_slot: 116, assertions_mapping_slot: 117 };
}

/// The state an assertion claims the L2 reached. `abi.encode` of this is what the assertion
/// hash commits to, so the field order here is the ABI order and cannot be rearranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AssertionState {
    /// `globalState.bytes32Vals[0]` - the L2 block hash this assertion ends at.
    pub l2_block_hash: B256,
    /// `globalState.bytes32Vals[1]` - the outbox send root.
    pub send_root: B256,
    /// `globalState.u64Vals`.
    pub inbox_position: u64,
    pub position_in_message: u64,
    pub machine_status: u8,
    pub end_history_root: B256,
}

/// `keccak(abi.encode(state))`, the inner hash the assertion hash is built from.
pub fn hash_assertion_state(state: &AssertionState) -> B256 {
    let mut encoded = [0u8; 192];
    encoded[0..32].copy_from_slice(state.l2_block_hash.as_slice());
    encoded[32..64].copy_from_slice(state.send_root.as_slice());
    encoded[88..96].copy_from_slice(&state.inbox_position.to_be_bytes());
    encoded[120..128].copy_from_slice(&state.position_in_message.to_be_bytes());
    encoded[159] = state.machine_status;
    encoded[160..192].copy_from_slice(state.end_history_root.as_slice());
    keccak256(encoded)
}

/// `keccak(prev || keccak(abi.encode(afterState)) || inboxAcc)`, as `RollupLib.assertionHash`
/// builds it. Reproducing it is what ties a supplied L2 block hash to the hash L1 stores.
pub fn get_assertion_hash(proof: &ArbitrumRootProof) -> B256 {
    let mut preimage = [0u8; 96];
    preimage[0..32].copy_from_slice(proof.prev_assertion_hash.as_slice());
    preimage[32..64].copy_from_slice(hash_assertion_state(&proof.after_state).as_slice());
    preimage[64..96].copy_from_slice(proof.inbox_accumulator.as_slice());
    keccak256(preimage)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArbitrumRootProof {
    pub rollup: Address,
    pub layout: RollupLayout,
    pub account: ClaimedAccount,
    pub account_proof: Vec<Bytes>,
    /// Proof of `_latestConfirmed`.
    pub latest_confirmed_proof: Vec<Bytes>,
    /// Proof of `_assertions[latestConfirmed]`, whose packed slot carries the status.
    pub assertion_node_proof: Vec<Bytes>,
    pub assertion_node_slot_value: U256,
    /// Preimage of the assertion hash L1 stores.
    pub prev_assertion_hash: B256,
    pub after_state: AssertionState,
    pub inbox_accumulator: B256,
    /// RLP of the L2 block header, whose keccak is `after_state.l2_block_hash`.
    pub l2_header_rlp: Bytes,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ArbitrumError {
    #[error(transparent)]
    Mpt(#[from] MptError),
    #[error("assertion preimage hashes to {got}, but L1 confirmed {expected}")]
    AssertionMismatch { got: B256, expected: B256 },
    #[error("assertion {hash} has status {status}, not confirmed")]
    NotConfirmed { hash: B256, status: u8 },
    #[error("L2 header RLP hashes to {got}, but the assertion committed to {expected}")]
    HeaderHashMismatch { got: B256, expected: B256 },
    #[error("L2 header RLP is malformed")]
    MalformedHeader,
}

/// Storage slot of `_assertions[hash]`: `keccak(hash || pad32(slot))`.
pub fn get_assertion_node_slot(hash: B256, layout: &RollupLayout) -> B256 {
    let mut preimage = [0u8; 64];
    preimage[0..32].copy_from_slice(hash.as_slice());
    preimage[56..64].copy_from_slice(&layout.assertions_mapping_slot.to_be_bytes());
    keccak256(preimage)
}

/// Pull `AssertionNode.status` out of its packed slot.
pub fn read_assertion_status(slot_value: U256) -> u8 {
    let shifted = slot_value >> (ASSERTION_STATUS_BYTE_OFFSET * 8);
    (shifted & U256::from(0xffu8)).to::<u8>()
}

/// Derive Arbitrum's L2 state root from a verified Ethereum L1 state root.
///
/// Three links, each checked rather than trusted: L1 storage says which assertion is
/// confirmed, the assertion's preimage says which L2 block it ends at, and the L2 header's
/// preimage says what that block's state root is.
pub fn get_arbitrum_root(
    l1_state_root: B256,
    proof: &ArbitrumRootProof,
) -> Result<AttestedRoot, ArbitrumError> {
    let storage_root =
        verify_account_proof(l1_state_root, proof.rollup, &proof.account, &proof.account_proof)?;

    // Which assertion is confirmed is read from L1, never supplied.
    let confirmed = get_assertion_hash(proof);
    let latest_slot = B256::from(U256::from(proof.layout.latest_confirmed_slot));
    verify_storage_proof(
        storage_root,
        latest_slot,
        U256::from_be_bytes(confirmed.0),
        &proof.latest_confirmed_proof,
    )
    .map_err(|_| ArbitrumError::AssertionMismatch { got: confirmed, expected: confirmed })?;

    // A pending assertion is still inside its challenge window and proves nothing.
    verify_storage_proof(
        storage_root,
        get_assertion_node_slot(confirmed, &proof.layout),
        proof.assertion_node_slot_value,
        &proof.assertion_node_proof,
    )?;
    let status = read_assertion_status(proof.assertion_node_slot_value);
    if status != ASSERTION_CONFIRMED {
        return Err(ArbitrumError::NotConfirmed { hash: confirmed, status });
    }

    let header = decode_l2_header(&proof.l2_header_rlp)?;
    let got = keccak256(&proof.l2_header_rlp);
    if got != proof.after_state.l2_block_hash {
        return Err(ArbitrumError::HeaderHashMismatch {
            got,
            expected: proof.after_state.l2_block_hash,
        });
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
