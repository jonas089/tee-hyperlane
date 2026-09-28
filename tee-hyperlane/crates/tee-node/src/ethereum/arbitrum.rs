//! Arbitrum Sepolia: its state root, read out of Ethereum.
//!
//! Arbitrum (BoLD) stores the hash of its latest confirmed *assertion* in L1 storage. The
//! assertion's preimage names the L2 block it ends at, and that block's header names its state
//! root. So once Ethereum is verified, Arbitrum's root is three checked links away.
//!
//! Only a *confirmed* assertion is trustless, and confirmation waits out the challenge window.

use crate::state::IsmState;
use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use serde_json::Value;

use super::l2_shared::{l2_head, Header, Input};
use super::Ethereum;
use crate::evm::{self, ClaimedAccount};
use crate::origin::{self, Chain, Head, Origin, Tree};

pub static ARBITRUM: Chain = Chain {
    name: "arbitrum",
    domain: 421614,
    origin: &Arbitrum,
};

// Where Arbitrum's state lives on L1, pinned rather than accepted: a proof against a
// caller-named contract or slot proves nothing.
/// The live BoLD rollup, `inbox.bridge().rollup()`. The widely listed `0xd808…81C8` is the
/// deprecated pre-BoLD contract, which still answers but has stopped confirming.
pub const ROLLUP: Address = address!("042B2E6C5E99d4c521bd49beeD5E99651D9B0Cf4");
/// `_latestConfirmed` and the `_assertions` mapping in `RollupCore`, read off live storage.
pub const LATEST_CONFIRMED_SLOT: u64 = 116;
pub const ASSERTIONS_SLOT: u64 = 117;
/// `AssertionNode.status` sits 25 bytes up its packed slot; 2 means confirmed.
const STATUS_BYTE: u32 = 25;
const CONFIRMED: u8 = 2;
/// Arbitrum's `MerkleTreeHook` keeps its tree from slot 151.
pub const TREE_SLOT: u64 = 151;

pub struct Arbitrum;

/// Everything that reads Arbitrum's confirmed root out of L1 storage.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RootProof {
    pub account: ClaimedAccount,
    pub account_proof: Vec<Bytes>,
    /// Proof of `_latestConfirmed`.
    pub latest_confirmed_proof: Vec<Bytes>,
    /// Proof of `_assertions[latestConfirmed]`, whose packed slot carries the status.
    pub assertion_node_proof: Vec<Bytes>,
    pub assertion_node_slot_value: U256,
    /// The preimage of the assertion hash L1 stores.
    pub prev_assertion_hash: B256,
    pub after_state: AssertionState,
    pub inbox_accumulator: B256,
    /// RLP of the L2 block header, whose keccak is `after_state.l2_block_hash`.
    pub l2_header_rlp: Bytes,
}

/// The state an assertion claims the L2 reached. Field order is the ABI order the assertion
/// hash commits to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AssertionState {
    pub l2_block_hash: B256,
    pub send_root: B256,
    pub inbox_position: u64,
    pub position_in_message: u64,
    pub machine_status: u8,
    pub end_history_root: B256,
}

impl Origin for Arbitrum {
    /// Verify Ethereum, then follow three links from its state root to Arbitrum's: L1 storage
    /// names the confirmed assertion, the assertion's preimage names the L2 block, and the
    /// block's header names the state root.
    fn verify(&self, input: Value, trusted: &IsmState) -> anyhow::Result<Head> {
        let Input { ethereum, proof } = origin::parse::<Input<RootProof>>("arbitrum input", input)?;
        let l1 = Ethereum.verify(ethereum, trusted)?;
        let rollup =
            evm::verify_account_proof(l1.root, ROLLUP, &proof.account, &proof.account_proof)?;

        let confirmed = assertion_hash(&proof);
        evm::verify_storage_proof(
            rollup,
            B256::from(U256::from(LATEST_CONFIRMED_SLOT)),
            U256::from_be_bytes(confirmed.0),
            &proof.latest_confirmed_proof,
        )
        .map_err(|_| anyhow::anyhow!("assertion {confirmed} is not the one L1 has confirmed"))?;

        // A pending assertion is still inside its challenge window and proves nothing.
        let mut node_slot = [0u8; 64];
        node_slot[..32].copy_from_slice(confirmed.as_slice());
        node_slot[56..].copy_from_slice(&ASSERTIONS_SLOT.to_be_bytes());
        evm::verify_storage_proof(
            rollup,
            keccak256(node_slot),
            proof.assertion_node_slot_value,
            &proof.assertion_node_proof,
        )?;
        let status = assertion_status(proof.assertion_node_slot_value);
        anyhow::ensure!(
            status == CONFIRMED,
            "assertion {confirmed} has status {status}, not confirmed"
        );

        anyhow::ensure!(
            keccak256(&proof.l2_header_rlp) == proof.after_state.l2_block_hash,
            "the L2 header is not the block the assertion commits to"
        );
        Ok(l2_head(l1, Header::decode(&proof.l2_header_rlp)?))
    }

    fn merkle_tree(&self, proof: Value, root: B256) -> anyhow::Result<Tree> {
        evm::read_tree(proof, root, TREE_SLOT)
    }
}

/// `AssertionNode.status`, out of the packed slot it shares with three block numbers and a flag.
fn assertion_status(slot_value: U256) -> u8 {
    (slot_value >> (STATUS_BYTE * 8) & U256::from(0xffu8)).to::<u8>()
}

/// `keccak(prev || keccak(abi.encode(afterState)) || inboxAcc)`, as `RollupLib` builds it.
fn assertion_hash(proof: &RootProof) -> B256 {
    let s = &proof.after_state;
    let mut state = [0u8; 192];
    state[0..32].copy_from_slice(s.l2_block_hash.as_slice());
    state[32..64].copy_from_slice(s.send_root.as_slice());
    state[88..96].copy_from_slice(&s.inbox_position.to_be_bytes());
    state[120..128].copy_from_slice(&s.position_in_message.to_be_bytes());
    state[159] = s.machine_status;
    state[160..192].copy_from_slice(s.end_history_root.as_slice());

    let mut preimage = [0u8; 96];
    preimage[0..32].copy_from_slice(proof.prev_assertion_hash.as_slice());
    preimage[32..64].copy_from_slice(keccak256(state).as_slice());
    preimage[64..96].copy_from_slice(proof.inbox_accumulator.as_slice());
    keccak256(preimage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> B256 {
        s.parse().unwrap()
    }

    /// Arbitrum Sepolia assertion `0x21b8…e543`, confirmed on L1, rebuilt from its
    /// `AssertionCreated` payload. A different L2 block cannot keep the hash L1 stores.
    #[test]
    fn the_assertion_hash_reproduces_what_l1_confirmed() {
        let mut proof = RootProof {
            account: ClaimedAccount {
                nonce: 0,
                balance: U256::ZERO,
                storage_root: B256::ZERO,
                code_hash: B256::ZERO,
            },
            account_proof: vec![],
            latest_confirmed_proof: vec![],
            assertion_node_proof: vec![],
            assertion_node_slot_value: U256::ZERO,
            prev_assertion_hash: h(
                "0x1d3df2803af5505c47893636c2b17a8ff955ff5336755aa990caf3baf9603b37",
            ),
            after_state: AssertionState {
                l2_block_hash: h(
                    "0x5cfbea5cf869e3cb10c27fdc898c48c9e03e035cf7ccc83cd654b6679fab88f9",
                ),
                send_root: h("0x6a72af1e7b61faf26be37e52b53622084b38ab56229903c5298a56cdbacc2298"),
                inbox_position: 0xcaba1,
                position_in_message: 0,
                machine_status: 1,
                end_history_root: h(
                    "0x972472422534efc53574219bf39d6c63f0887c07edba578a2ac38bea2d1cdebb",
                ),
            },
            inbox_accumulator: h(
                "0xcda07eb616939141ec565f6e837e62c723199d670402bb4791789e33149bfbb7",
            ),
            l2_header_rlp: Bytes::new(),
        };
        let confirmed = h("0x21b8c3b857fa797973b6693befba28f6e61aed1b35fe2a6d7ecce238a646e543");
        assert_eq!(assertion_hash(&proof), confirmed);
        proof.after_state.l2_block_hash = B256::ZERO;
        assert_ne!(assertion_hash(&proof), confirmed);
    }

    /// firstChildBlock | secondChildBlock | createdAtBlock | isFirstChild | status, read off
    /// a live confirmed node and the same node while it was still pending.
    #[test]
    fn only_a_confirmed_assertion_reads_as_confirmed() {
        let confirmed = "0x00000000000002010000000000b1de2c00000000000000000000000000b1dec9";
        let pending = "0x00000000000001010000000000b1de2c00000000000000000000000000b1dec9";
        assert_eq!(assertion_status(confirmed.parse().unwrap()), CONFIRMED);
        assert_eq!(assertion_status(pending.parse().unwrap()), 1);
    }
}
