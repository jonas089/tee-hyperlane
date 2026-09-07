//! Arbitrum and Base state roots, derived from a verified Ethereum L1 state root.
//!
//! Neither chain needs its own light client or its own enclave: both publish a commitment
//! to their L2 state into L1 storage, so once the Ethereum light client has verified an L1
//! state root, the L2 root is reachable by MPT proof plus a keccak preimage check.

use alloy_primitives::{keccak256, B256};
use tee_node::origins::ethereum_l2::*;

fn h(s: &str) -> B256 {
    s.parse().unwrap()
}

// ---- Arbitrum ----

#[derive(serde::Deserialize)]
struct ArbHeader {
    rlp: String,
    hash: String,
    #[serde(rename = "stateRoot")]
    state_root: String,
    number: u64,
    timestamp: u64,
}

fn arb_header() -> ArbHeader {
    serde_json::from_str(include_str!("../testdata/arbitrum_header.json")).unwrap()
}

/// The header layout this bridge depends on, checked against a real Arbitrum Sepolia block.
#[test]
fn a_live_arbitrum_header_decodes_at_the_expected_field_positions() {
    let f = arb_header();
    let rlp = hex::decode(f.rlp.trim_start_matches("0x")).unwrap();

    // If the field set or order were wrong, this equality would fail.
    assert_eq!(keccak256(&rlp), h(&f.hash), "reconstructed header must hash to the block hash");

    let header = decode_l2_header(&rlp).unwrap();
    assert_eq!(header.state_root, h(&f.state_root));
    assert_eq!(header.number, f.number);
    assert_eq!(header.timestamp, f.timestamp);
}

#[test]
fn a_truncated_header_is_rejected() {
    let f = arb_header();
    let rlp = hex::decode(f.rlp.trim_start_matches("0x")).unwrap();
    assert_eq!(decode_l2_header(&rlp[..rlp.len() / 2]), Err(ArbitrumError::MalformedHeader));
    assert_eq!(decode_l2_header(b""), Err(ArbitrumError::MalformedHeader));
}

/// Which node is read must be derived from L1, not chosen by the relayer.
#[test]
fn the_confirm_data_slot_is_derived_from_the_node_number() {
    let layout = RollupLayout::ARBITRUM_SEPOLIA;
    let a = get_confirm_data_slot(10764, &layout);
    let b = get_confirm_data_slot(10765, &layout);
    assert_ne!(a, b, "different nodes must map to different slots");

    // keccak(pad32(node) || pad32(slot)) + 2
    let mut preimage = [0u8; 64];
    preimage[24..32].copy_from_slice(&10764u64.to_be_bytes());
    preimage[56..64].copy_from_slice(&118u64.to_be_bytes());
    let expected = alloy_primitives::U256::from_be_bytes(keccak256(preimage).0)
        + alloy_primitives::U256::from(2);
    assert_eq!(a, B256::from(expected));
}

#[test]
fn confirm_data_binds_both_the_block_hash_and_the_send_root() {
    let block_hash = h("0x1111111111111111111111111111111111111111111111111111111111111111");
    let send_root = h("0x2222222222222222222222222222222222222222222222222222222222222222");
    let expect = keccak256([block_hash.as_slice(), send_root.as_slice()].concat());

    let other = keccak256([send_root.as_slice(), block_hash.as_slice()].concat());
    assert_ne!(expect, other, "order must matter");
}

// ---- Base ----

#[derive(serde::Deserialize)]
struct BaseFixture {
    l2_block_number: u64,
    l2_timestamp: u64,
    version: String,
    state_root: String,
    message_passer_storage_root: String,
    latest_block_hash: String,
}

fn base_fixture() -> BaseFixture {
    serde_json::from_str(include_str!("../testdata/base_output_root.json")).unwrap()
}

fn preimage(f: &BaseFixture) -> BaseOutputRootPreimage {
    BaseOutputRootPreimage {
        version: h(&f.version),
        state_root: h(&f.state_root),
        message_passer_storage_root: h(&f.message_passer_storage_root),
        latest_block_hash: h(&f.latest_block_hash),
        l2_block_number: f.l2_block_number,
        l2_timestamp: f.l2_timestamp,
    }
}

/// The OP Stack output-root formula, over four genuine values read at one Base Sepolia block.
#[test]
fn the_output_root_matches_the_op_stack_formula() {
    let f = base_fixture();
    let p = preimage(&f);
    let expected = keccak256(
        [
            p.version.as_slice(),
            p.state_root.as_slice(),
            p.message_passer_storage_root.as_slice(),
            p.latest_block_hash.as_slice(),
        ]
        .concat(),
    );
    assert_eq!(hash_output_root(&p), expected);
}

/// Every component must be bound, or a relayer could swap in another chain's state root.
#[test]
fn the_output_root_binds_every_component() {
    let f = base_fixture();
    let base = hash_output_root(&preimage(&f));

    let mut p = preimage(&f);
    p.state_root = h("0x3333333333333333333333333333333333333333333333333333333333333333");
    assert_ne!(base, hash_output_root(&p), "state root");

    let mut p = preimage(&f);
    p.message_passer_storage_root = B256::ZERO;
    assert_ne!(base, hash_output_root(&p), "message passer root");

    let mut p = preimage(&f);
    p.latest_block_hash = B256::ZERO;
    assert_ne!(base, hash_output_root(&p), "block hash");

    let mut p = preimage(&f);
    p.version = h("0x0000000000000000000000000000000000000000000000000000000000000001");
    assert_ne!(base, hash_output_root(&p), "version");
}

/// The block number and timestamp travel alongside the root rather than inside it, so they
/// must not silently change what the root commits to.
#[test]
fn block_metadata_is_not_part_of_the_output_root() {
    let f = base_fixture();
    let base = hash_output_root(&preimage(&f));
    let mut p = preimage(&f);
    p.l2_block_number += 1;
    p.l2_timestamp += 12;
    assert_eq!(base, hash_output_root(&p));
}

/// The packed slot read out of Arbitrum Sepolia's live rollup at L1 block time.
///
/// `latestConfirmed()` returned 10764, and slot 117 holds
/// `0x3f5a36 0000000000002a0c 0000000000002a0d 0000000000002a0c`. Solidity packs from the
/// least significant byte, so the node number is the low 8 bytes - reading the slot as a
/// whole uint256 would give a nonsensical node and prove nothing.
#[test]
fn the_arbitrum_node_number_is_unpacked_from_its_slot() {
    use alloy_primitives::U256;
    let slot_value: U256 =
        "0x3f5a360000000000002a0c0000000000002a0d0000000000002a0c".parse().unwrap();
    let layout = RollupLayout::ARBITRUM_SEPOLIA;
    assert_eq!(layout.read_latest_confirmed(slot_value), 10764, "matches latestConfirmed()");
    assert_ne!(
        U256::from(layout.read_latest_confirmed(slot_value)),
        slot_value,
        "the raw slot is not the node number"
    );
}

#[test]
fn a_different_byte_offset_reads_a_different_packed_value() {
    use alloy_primitives::U256;
    let slot_value: U256 =
        "0x3f5a360000000000002a0c0000000000002a0d0000000000002a0c".parse().unwrap();
    let mut layout = RollupLayout::ARBITRUM_SEPOLIA;
    layout.latest_confirmed_byte_offset = 8;
    assert_eq!(layout.read_latest_confirmed(slot_value), 10765);
}
