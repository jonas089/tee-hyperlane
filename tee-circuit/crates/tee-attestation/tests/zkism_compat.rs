//! Byte-level compatibility with Celestia's `x/zkism`, plus the ISM state we layer on top.
//!
//! The fixtures come from celestia-app v9.0.6 - the version live Mocha runs - so they are
//! the exact bytes its Go decoder accepts. Drift here rejects every proof the bridge
//! submits, which is why these are golden tests rather than round-trip tests alone.

use tee_attestation::*;

const STATE_TRANSITION_PV: &[u8] =
    include_bytes!("../testdata/zkism/state_transition_public_values.bin");
const STATE_MEMBERSHIP_PV: &[u8] =
    include_bytes!("../testdata/zkism/state_membership_public_values.bin");
const GROTH16_VK_V5: &[u8] = include_bytes!("../testdata/zkism/groth16_vk_v5.bin");

// ---- public values ----

#[test]
fn state_transition_fixture_round_trips_byte_for_byte() {
    let v = decode_state_transition_values(STATE_TRANSITION_PV).expect("decode");
    assert_eq!(STATE_TRANSITION_PV.len(), 298, "8 + 141 + 8 + 141");
    assert_eq!((v.state.len(), v.new_state.len()), (141, 141));
    assert_eq!(encode_state_transition_values(&v.state, &v.new_state), STATE_TRANSITION_PV);
}

#[test]
fn state_membership_fixture_round_trips_byte_for_byte() {
    let v = decode_state_membership_values(STATE_MEMBERSHIP_PV).expect("decode");
    assert_eq!(STATE_MEMBERSHIP_PV.len(), 104, "32 + 32 + 8 + 32");
    assert_eq!(v.message_ids.len(), 1);
    assert_eq!(
        encode_state_membership_values(v.state_root, v.merkle_tree_address, &v.message_ids),
        STATE_MEMBERSHIP_PV
    );
}

#[test]
fn message_count_prefix_is_little_endian() {
    // A big-endian slip would put the 1 at offset 71 instead of 64.
    assert_eq!(&STATE_MEMBERSHIP_PV[64..72], &[1, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn live_mocha_groth16_wrap_vk_is_the_sp1_v5_shape() {
    // v9.0.6 ValidateGroth16Vkey: exactly 396 bytes, G1.K length 3 (2 public inputs + 1),
    // no commitment keys. SP1 v6's 492-byte, 5-input key is rejected there.
    assert_eq!(GROTH16_VK_V5.len(), 396);
    assert_eq!(u32::from_be_bytes(GROTH16_VK_V5[288..292].try_into().unwrap()), 3);
    assert_eq!(u32::from_be_bytes(GROTH16_VK_V5[388..392].try_into().unwrap()), 0);
    assert_eq!(u32::from_be_bytes(GROTH16_VK_V5[392..396].try_into().unwrap()), 0);
}

#[test]
fn membership_decoder_rejects_trailing_and_truncated_input() {
    let mut long = STATE_MEMBERSHIP_PV.to_vec();
    long.push(0);
    assert!(matches!(
        decode_state_membership_values(&long),
        Err(CodecError::TrailingBytes { .. })
    ));
    let short = &STATE_MEMBERSHIP_PV[..STATE_MEMBERSHIP_PV.len() - 1];
    assert!(matches!(decode_state_membership_values(short), Err(CodecError::TooShort { .. })));
}

#[test]
fn transition_decoder_rejects_out_of_range_state_length() {
    let mut b = 31u64.to_le_bytes().to_vec();
    b.extend_from_slice(&[0u8; 31]);
    assert!(matches!(
        decode_state_transition_values(&b),
        Err(CodecError::StateLengthOutOfRange(31))
    ));
}

/// Why the bridge needs two proofs rather than one.
///
/// Submitting one blob to both `MsgUpdateInterchainSecurityModule` and `MsgSubmitMessages`
/// would need it to decode as both shapes. The transition decoder reads bytes 0..8 as a
/// length that must land in 32..=2048, while the membership decoder needs those same bytes
/// to be the start of a real state root. A hash essentially never satisfies both.
#[test]
fn one_blob_cannot_serve_both_zkism_handlers() {
    let claimed_len = u64::from_le_bytes(STATE_MEMBERSHIP_PV[..8].try_into().unwrap());
    assert!(claimed_len > 2048, "state root began with a plausible length prefix");
    assert!(decode_state_transition_values(STATE_MEMBERSHIP_PV).is_err());
}

// ---- ISM state ----

fn state(root: u8, height: u64, ts: u64) -> IsmState {
    IsmState {
        state_root: [root; 32],
        origin_domain: 11155111,
        height,
        timestamp: ts,
        lc_store_commit: [7u8; 32],
        identity_digest: [9u8; 32],
    }
}

#[test]
fn ism_state_round_trips_and_fits_the_zkism_window() {
    let s = state(1, 100, 1_700_000_000);
    let b = encode_ism_state(&s);
    assert_eq!(b.len(), 116);
    assert!((32..=2048).contains(&b.len()));
    assert_eq!(decode_ism_state(&b).unwrap(), s);
    assert_eq!(&b[..32], &s.state_root, "zkism reads state[..32] as the root");
}

#[test]
fn ism_state_decode_rejects_wrong_length() {
    assert!(matches!(
        decode_ism_state(&[0u8; 115]),
        Err(CodecError::WrongLength { expected: 116, got: 115 })
    ));
}

#[test]
fn a_valid_transition_is_accepted() {
    assert!(check_transition(&state(1, 10, 100), &state(2, 11, 101)).is_ok());
}

#[test]
fn a_transition_must_change_the_state_root() {
    // Otherwise x/zkism never re-arms submissions[ismId] and message delivery wedges.
    assert_eq!(
        check_transition(&state(1, 10, 100), &state(1, 11, 101)),
        Err(TransitionError::StateRootUnchanged)
    );
}

#[test]
fn a_transition_must_advance_height_and_not_rewind_time() {
    assert_eq!(
        check_transition(&state(1, 10, 100), &state(2, 10, 101)),
        Err(TransitionError::HeightNotAdvanced)
    );
    assert_eq!(
        check_transition(&state(1, 10, 100), &state(2, 11, 99)),
        Err(TransitionError::TimestampWentBackwards)
    );
}

/// What stops an Ethereum attestation being replayed into a Celestia-origin ISM when both
/// share one state_transition_vkey.
#[test]
fn a_transition_pins_the_origin_domain_and_enclave_identity() {
    let mut other_origin = state(2, 11, 101);
    other_origin.origin_domain = 1297040200;
    assert_eq!(
        check_transition(&state(1, 10, 100), &other_origin),
        Err(TransitionError::OriginDomainChanged)
    );

    let mut other_enclave = state(2, 11, 101);
    other_enclave.identity_digest = [0u8; 32];
    assert_eq!(
        check_transition(&state(1, 10, 100), &other_enclave),
        Err(TransitionError::IdentityChanged)
    );
}

// ---- attested payload ----

fn update(ids: Vec<[u8; 32]>) -> AttestedUpdate {
    AttestedUpdate {
        prev_state: state(1, 10, 100),
        new_state: state(2, 11, 101),
        merkle_tree_address: [0x5a; 32],
        message_ids: ids,
    }
}

#[test]
fn attested_update_round_trips_with_and_without_messages() {
    for ids in [vec![], vec![[1u8; 32], [2u8; 32], [3u8; 32]]] {
        let u = update(ids);
        let b = encode_attested_update(&u);
        assert_eq!(decode_attested_update(&b).unwrap(), u);
    }
}

#[test]
fn attested_update_decoder_rejects_trailing_bytes() {
    let mut b = encode_attested_update(&update(vec![[1u8; 32]]));
    b.push(0);
    assert!(matches!(decode_attested_update(&b), Err(CodecError::TrailingBytes { .. })));
}

#[test]
fn the_payload_hash_binds_every_field() {
    let base = update(vec![[1u8; 32]]);
    let h = hash_attested_update(&base);

    let mut m = base.clone();
    m.message_ids = vec![[2u8; 32]];
    assert_ne!(h, hash_attested_update(&m), "message ids");

    let mut m = base.clone();
    m.merkle_tree_address = [0x5b; 32];
    assert_ne!(h, hash_attested_update(&m), "merkle tree address");

    let mut m = base.clone();
    m.new_state.state_root = [0xff; 32];
    assert_ne!(h, hash_attested_update(&m), "new state");

    let mut m = base.clone();
    m.prev_state.height = 9;
    assert_ne!(h, hash_attested_update(&m), "prev state");

    // Order matters: it mirrors the origin tree's insert order.
    assert_ne!(
        hash_attested_update(&update(vec![[1u8; 32], [2u8; 32]])),
        hash_attested_update(&update(vec![[2u8; 32], [1u8; 32]]))
    );
}
