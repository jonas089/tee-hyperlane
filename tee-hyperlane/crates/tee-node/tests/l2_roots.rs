//! Arbitrum and Base state roots, derived from a verified Ethereum L1 state root.
//!
//! Neither chain needs its own light client or its own enclave: both publish a commitment
//! to their L2 state into L1 storage, so once the Ethereum light client has verified an L1
//! state root, the L2 root is reachable by MPT proof plus a keccak preimage check.

use alloy_primitives::{keccak256, B256};
use tee_node::origins::ethereum_l2::*;
use tee_node::state_proofs::ClaimedAccount;

fn h(s: &str) -> B256 {
    s.parse().unwrap()
}

// ---- Arbitrum ----

/// Arbitrum Sepolia assertion `0x21b8…e543` and its parent, both confirmed on L1 Sepolia.
/// The preimage below is the `AssertionCreated` payload for that assertion.
const CONFIRMED_ASSERTION: &str =
    "0x21b8c3b857fa797973b6693befba28f6e61aed1b35fe2a6d7ecce238a646e543";
const PARENT_ASSERTION: &str =
    "0x1d3df2803af5505c47893636c2b17a8ff955ff5336755aa990caf3baf9603b37";

fn confirmed_assertion_proof() -> ArbitrumRootProof {
    ArbitrumRootProof {
        account: ClaimedAccount {
            nonce: 0,
            balance: Default::default(),
            storage_root: B256::ZERO,
            code_hash: B256::ZERO,
        },
        account_proof: Vec::new(),
        latest_confirmed_proof: Vec::new(),
        assertion_node_proof: Vec::new(),
        assertion_node_slot_value: "0x00000000000002010000000000b1de2c00000000000000000000000000b1dec9"
            .parse()
            .unwrap(),
        prev_assertion_hash: h(PARENT_ASSERTION),
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
        l2_header_rlp: Default::default(),
    }
}

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

/// Which assertion is read must be derived from L1, not chosen by the relayer.
#[test]
fn the_assertion_node_slot_is_derived_from_the_assertion_hash() {
    let layout = RollupLayout::ARBITRUM_SEPOLIA;
    let a = get_assertion_node_slot(h(CONFIRMED_ASSERTION), &layout);
    let b = get_assertion_node_slot(h(PARENT_ASSERTION), &layout);
    assert_ne!(a, b, "different assertions must map to different slots");

    // keccak(hash || pad32(slot))
    let mut preimage = [0u8; 64];
    preimage[0..32].copy_from_slice(h(CONFIRMED_ASSERTION).as_slice());
    preimage[56..64].copy_from_slice(&117u64.to_be_bytes());
    assert_eq!(a, keccak256(preimage));
}

/// The assertion hash is built from the whole after-state, so a relayer cannot swap in a
/// different L2 block and keep the hash L1 stores.
///
/// Values are Arbitrum Sepolia assertion
/// `0x21b8c3b857fa797973b6693befba28f6e61aed1b35fe2a6d7ecce238a646e543`, confirmed on L1.
#[test]
fn the_assertion_hash_reproduces_what_l1_confirmed() {
    let proof = confirmed_assertion_proof();
    assert_eq!(get_assertion_hash(&proof), h(CONFIRMED_ASSERTION));

    let mut tampered = proof.clone();
    tampered.after_state.l2_block_hash = h(PARENT_ASSERTION);
    assert_ne!(get_assertion_hash(&tampered), h(CONFIRMED_ASSERTION));
}

#[test]
fn only_a_confirmed_assertion_is_accepted() {
    use alloy_primitives::U256;
    // firstChildBlock | secondChildBlock | createdAtBlock | isFirstChild | status
    let confirmed: U256 =
        "0x00000000000002010000000000b1de2c00000000000000000000000000b1dec9".parse().unwrap();
    assert_eq!(read_assertion_status(confirmed), 2);

    let pending: U256 =
        "0x00000000000001010000000000b1de2c00000000000000000000000000b1dec9".parse().unwrap();
    assert_eq!(read_assertion_status(pending), 1, "pending is still in its challenge window");
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
    header_rlp: String,
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

/// The output root commits to the block *hash* and not to its height or its time, which is
/// exactly why neither may be taken from the preimage: a caller could put anything there and
/// the root would still match. `get_base_root` reads both out of the header instead, and the
/// header is bound by that hash.
///
#[test]
fn the_l2_header_supplies_base_height_and_time() {
    let f = base_fixture();
    let rlp = hex::decode(f.header_rlp.trim_start_matches("0x")).unwrap();

    // The link that makes them trustworthy: this header is the one the root committed to.
    assert_eq!(keccak256(&rlp), h(&f.latest_block_hash));

    let header = decode_l2_header(&rlp).unwrap();
    assert_eq!(header.number, f.l2_block_number);
    assert_eq!(header.timestamp, f.l2_timestamp);
    assert_eq!(header.state_root, h(&f.state_root));
}

/// A header from a different block cannot be substituted, even though the four values the
/// output root commits to are unchanged - which is the whole reason height and time are read
/// from the header rather than from the preimage.
#[test]
fn a_header_that_does_not_match_the_committed_hash_is_useless() {
    let f = base_fixture();
    let mut rlp = hex::decode(f.header_rlp.trim_start_matches("0x")).unwrap();
    let last = rlp.len() - 1;
    rlp[last] ^= 0x01;
    assert_ne!(keccak256(&rlp), h(&f.latest_block_hash));
}
