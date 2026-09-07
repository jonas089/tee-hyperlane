//! What must hold after the quote's signature checks out: the payload is really attested,
//! the transition is legal, and the prover's clock is anchored to attested chain time.

mod common;

use common::*;
use tee_attestation::*;

fn log_json(log: &[EventLog]) -> Vec<u8> {
    serde_json::to_vec(log).unwrap()
}

fn run(u: &AttestedUpdate, now: u64) -> Result<AttestedUpdate, AttestationError> {
    let log = good_log();
    check_attested_report(
        &report_for(&log, u),
        &log_json(&log),
        &encode_attested_update(u),
        now,
        &policy(),
    )
}

#[test]
fn a_well_formed_attestation_is_accepted() {
    let u = update();
    assert_eq!(run(&u, HEAD_TS).unwrap(), u);
}

#[test]
fn a_payload_the_quote_did_not_commit_to_is_rejected() {
    let log = good_log();
    let u = update();
    let mut tampered = u.clone();
    tampered.new_state.state_root = [0xff; 32];
    assert_eq!(
        check_attested_report(
            &report_for(&log, &u),
            &log_json(&log),
            &encode_attested_update(&tampered),
            HEAD_TS,
            &policy(),
        ),
        Err(AttestationError::PayloadNotAttested)
    );
}

/// Swapping in a different message batch under the same quote must fail; this is what makes
/// one attestation safe to use for both proofs.
#[test]
fn the_message_batch_is_bound_to_the_quote() {
    let log = good_log();
    let u = update();
    let mut tampered = u.clone();
    tampered.message_ids = vec![[0xaa; 32]];
    assert_eq!(
        check_attested_report(
            &report_for(&log, &u),
            &log_json(&log),
            &encode_attested_update(&tampered),
            HEAD_TS,
            &policy(),
        ),
        Err(AttestationError::PayloadNotAttested)
    );
}

#[test]
fn unaccounted_report_data_padding_is_rejected() {
    use dcap_qvl::tcb_info::TcbStatus;
    let log = good_log();
    let u = update();
    let mut rd = [0u8; 64];
    rd[..32].copy_from_slice(&hash_attested_update(&u));
    rd[63] = 1;
    let r = report(&log, MR_TD, TcbStatus::UpToDate, rd);
    assert_eq!(
        check_attested_report(&r, &log_json(&log), &encode_attested_update(&u), HEAD_TS, &policy()),
        Err(AttestationError::ReportDataNotPadded)
    );
}

/// The lever an untrusted host would pull to revive a TCB Intel has since revoked.
#[test]
fn a_clock_rewound_below_the_attested_head_is_rejected() {
    let u = update();
    assert_eq!(
        run(&u, HEAD_TS - 1),
        Err(AttestationError::ClockBehindAttestedHead { now: HEAD_TS - 1, head: HEAD_TS })
    );
}

#[test]
fn a_clock_far_ahead_of_the_attested_head_is_rejected() {
    let u = update();
    let now = HEAD_TS + MAX_QUOTE_SKEW_SECS + 1;
    assert_eq!(
        run(&u, now),
        Err(AttestationError::ClockTooFarAhead { now, head: HEAD_TS })
    );
}

#[test]
fn clock_drift_inside_the_window_is_tolerated() {
    let u = update();
    assert!(run(&u, HEAD_TS + MAX_QUOTE_SKEW_SECS).is_ok());
}

#[test]
fn an_illegal_transition_is_rejected_even_when_properly_attested() {
    let mut u = update();
    u.new_state.state_root = u.prev_state.state_root;
    assert_eq!(
        run(&u, HEAD_TS),
        Err(AttestationError::Transition(TransitionError::StateRootUnchanged))
    );
}

#[test]
fn a_malformed_event_log_is_rejected() {
    let log = good_log();
    let u = update();
    assert_eq!(
        check_attested_report(
            &report_for(&log, &u),
            b"not json",
            &encode_attested_update(&u),
            HEAD_TS,
            &policy(),
        ),
        Err(AttestationError::EventLogMalformed)
    );
}

#[test]
fn a_truncated_payload_is_rejected() {
    let log = good_log();
    let u = update();
    let bytes = encode_attested_update(&u);
    assert_eq!(
        check_attested_report(
            &report_for(&log, &u),
            &log_json(&log),
            &bytes[..bytes.len() - 1],
            HEAD_TS,
            &policy(),
        ),
        Err(AttestationError::PayloadMalformed)
    );
}

/// An enclave that is not ours must be refused before the payload is even looked at.
#[test]
fn a_foreign_enclave_is_rejected_before_the_payload_matters() {
    let mut log = good_log();
    log[2] = ev(3, "compose-hash", &[0xde; 32]);
    let u = update();
    let err = check_attested_report(
        &report_for(&log, &u),
        &log_json(&log),
        &encode_attested_update(&u),
        HEAD_TS,
        &policy(),
    )
    .unwrap_err();
    assert!(matches!(err, AttestationError::Identity(IdentityError::EventMismatch(_))));
}

/// Regression test for an encoding bug that silently broke every proof.
///
/// `QuoteCollateralV3`'s serde implementation is not round-trip safe under bincode: what it
/// writes cannot be read back, failing with an unexpected EOF. Since SP1 serializes guest
/// stdin with bincode, that turned into "the guest cannot parse its own inputs". Carrying
/// the collateral as SCALE - the encoding the type derives natively - makes the round trip
/// total.
#[test]
fn collateral_survives_the_round_trip_into_a_guest() {
    let Some(dir) = sample_dir() else {
        eprintln!("dcap-qvl sample directory not found; skipping");
        return;
    };
    let quote = std::fs::read(dir.join("tdx_quote")).unwrap();
    let collateral: dcap_qvl::QuoteCollateralV3 =
        serde_json::from_slice(&std::fs::read(dir.join("tdx_quote_collateral.json")).unwrap())
            .unwrap();

    let inputs = AttestationInputs {
        quote,
        event_log: b"[]".to_vec(),
        collateral: AttestationInputs::encode_collateral(&collateral),
        now: 1_720_000_000,
        payload: Vec::new(),
    };

    // This is exactly what SP1 does with guest stdin.
    let bytes = bincode::serialize(&inputs).unwrap();
    let back: AttestationInputs = bincode::deserialize(&bytes).unwrap();
    assert_eq!(back.decode_collateral().unwrap(), collateral);
}

fn sample_dir() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("DCAP_SAMPLE_DIR") {
        return Some(std::path::PathBuf::from(dir));
    }
    let base = std::path::PathBuf::from(std::env::var("HOME").ok()?).join(".cargo/registry/src");
    std::fs::read_dir(base).ok()?.find_map(|e| {
        let p = e.ok()?.path().join("dcap-qvl-0.5.3/sample");
        p.is_dir().then_some(p)
    })
}
