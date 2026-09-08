//! The identity and platform checks `evolve-tee` omits.

mod common;

/// Locate dcap-qvl's sample quotes, if this machine has the crate vendored.
pub fn sample_dir_for_tests() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("DCAP_SAMPLE_DIR") {
        return Some(std::path::PathBuf::from(dir));
    }
    let base = std::path::PathBuf::from(std::env::var("HOME").ok()?).join(".cargo/registry/src");
    std::fs::read_dir(base).ok()?.find_map(|e| {
        let p = e.ok()?.path().join("dcap-qvl-0.5.3/sample");
        p.is_dir().then_some(p)
    })
}

use common::*;
use dcap_qvl::tcb_info::TcbStatus;
use tee_attestation::attestation::{event_preimage_v2, sha384, APPLICATION_IMR};
use tee_attestation::*;

fn check(log: &[EventLog], report: &dcap_qvl::verify::VerifiedReport) -> Result<(), IdentityError> {
    check_enclave_identity(&policy(), report, log, &replay_event_logs(log))
}

fn ok_report(log: &[EventLog]) -> dcap_qvl::verify::VerifiedReport {
    report(log, MR_TD, TcbStatus::UpToDate, [0u8; 64])
}

#[test]
fn our_enclave_is_accepted() {
    let log = good_log();
    assert_eq!(check(&log, &ok_report(&log)), Ok(()));
}

#[test]
fn a_different_container_image_is_rejected() {
    // The whole point: another TDX machine running other code must not pass.
    let mut log = good_log();
    log[2] = ev(APPLICATION_IMR, "compose-hash", &[0xde; 32]);
    assert_eq!(check(&log, &ok_report(&log)), Err(IdentityError::EventMismatch("compose-hash")));
}

#[test]
fn a_different_os_image_is_rejected() {
    let mut log = good_log();
    log[1] = ev(APPLICATION_IMR, "os-image-hash", &[0xde; 32]);
    assert_eq!(check(&log, &ok_report(&log)), Err(IdentityError::EventMismatch("os-image-hash")));
}

#[test]
fn a_different_platform_measurement_is_rejected() {
    let log = good_log();
    let r = report(&log, [0x22; 48], TcbStatus::UpToDate, [0u8; 64]);
    assert_eq!(check(&log, &r), Err(IdentityError::PlatformMeasurementMismatch));
}

#[test]
fn an_event_log_that_does_not_replay_to_the_quote_is_rejected() {
    let log = good_log();
    let r = ok_report(&log);
    let mut tampered = log.clone();
    tampered.push(ev(APPLICATION_IMR, "extra", b"junk"));
    assert_eq!(
        check_enclave_identity(&policy(), &r, &tampered, &replay_event_logs(&tampered)),
        Err(IdentityError::EventLogNotBoundToQuote)
    );
}

#[test]
fn platforms_intel_has_flagged_are_rejected() {
    for status in [
        TcbStatus::OutOfDate,
        TcbStatus::Revoked,
        TcbStatus::ConfigurationNeeded,
        TcbStatus::ConfigurationAndSWHardeningNeeded,
        TcbStatus::OutOfDateConfigurationNeeded,
    ] {
        let log = good_log();
        let r = report(&log, MR_TD, status.clone(), [0u8; 64]);
        assert!(
            matches!(check(&log, &r), Err(IdentityError::TcbNotAcceptable { .. })),
            "{status:?} should not be accepted"
        );
        assert!(matches!(check_platform_tcb(&r), Err(IdentityError::TcbNotAcceptable { .. })));
    }
}

#[test]
fn software_hardening_needed_is_accepted() {
    let log = good_log();
    let r = report(&log, MR_TD, TcbStatus::SWHardeningNeeded, [0u8; 64]);
    assert_eq!(check(&log, &r), Ok(()));
}

/// The attack `evolve-tee` leaves open: keep every genuine digest so the RTMR replay is
/// bit-identical, and only relabel the surrounding text to impersonate a pinned key.
#[test]
fn relabelled_event_text_is_rejected_even_though_the_rtmrs_still_match() {
    let log = good_log();
    let r = ok_report(&log);

    let mut forged = log.clone();
    forged[5].event = "compose-hash".to_string();
    forged[5].event_payload = vec![0xde; 32];

    assert_eq!(replay_event_logs(&forged), replay_event_logs(&log), "RTMRs must still match");
    assert_eq!(check(&forged, &r), Err(IdentityError::EventUnusable("compose-hash")));
}

#[test]
fn an_event_whose_digest_does_not_commit_to_its_text_is_unusable() {
    let mut log = good_log();
    log[2].event_payload = vec![0xde; 32]; // digest still belongs to the old payload
    assert_eq!(check(&log, &ok_report(&log)), Err(IdentityError::EventUnusable("compose-hash")));
}

#[test]
fn duplicated_keys_never_resolve() {
    let mut log = good_log();
    log.push(ev(APPLICATION_IMR, "compose-hash", &[0xde; 32]));
    assert_eq!(check(&log, &ok_report(&log)), Err(IdentityError::EventUnusable("compose-hash")));
}

#[test]
fn both_dstack_event_log_versions_are_accepted() {
    let payload = b"\xde\xad\xbe\xef";
    let mut e = ev(APPLICATION_IMR, "app-id", payload);
    assert!(event_digest_matches(&e), "v1 digest");
    e.digest = sha384(&event_preimage_v2(ET, "app-id", payload));
    assert!(event_digest_matches(&e), "v2 digest");
    e.digest = [0x99; 48];
    assert!(!event_digest_matches(&e), "unrelated digest must not match");
}

#[test]
fn v2_preimage_is_canonical_json_with_sorted_keys() {
    let s = String::from_utf8(event_preimage_v2(ET, "app-id", &[0xde, 0xad])).unwrap();
    assert_eq!(s, r#"{"name":"app-id","payload":"dead","type":134217729}"#);
}

// ---- development mode ----

/// `IdentityPolicy::Any` exists so the bridge is testable before a CVM exists. It must
/// still enforce everything that does not depend on knowing which enclave we run.
#[test]
fn development_mode_accepts_any_enclave_but_still_enforces_the_platform() {
    let mut log = good_log();
    log[2] = ev(APPLICATION_IMR, "compose-hash", &[0xde; 32]);
    let r = ok_report(&log);
    let replay = replay_event_logs(&log);

    assert_eq!(check_enclave_identity(&IdentityPolicy::Any, &r, &log, &replay), Ok(()));

    // ...but a bad TCB level is still refused.
    let revoked = report(&log, MR_TD, TcbStatus::Revoked, [0u8; 64]);
    assert!(matches!(
        check_enclave_identity(&IdentityPolicy::Any, &revoked, &log, &replay),
        Err(IdentityError::TcbNotAcceptable { .. })
    ));

    // ...and so is an event log the hardware did not measure.
    let mut tampered = log.clone();
    tampered.push(ev(APPLICATION_IMR, "extra", b"junk"));
    assert_eq!(
        check_enclave_identity(&IdentityPolicy::Any, &r, &tampered, &replay_event_logs(&tampered)),
        Err(IdentityError::EventLogNotBoundToQuote)
    );
}

/// A development ISM must be recognisable on chain, not silently indistinguishable from a
/// production one.
#[test]
fn development_and_production_identities_have_different_digests() {
    assert_ne!(IdentityPolicy::Any.digest(), policy().digest());
    assert!(!IdentityPolicy::Any.requires_specific_enclave());
    assert!(policy().requires_specific_enclave());
}

#[test]
fn the_identity_digest_covers_every_pinned_field() {
    let base = policy().digest();
    for mutate in [
        (|i: &mut EnclaveIdentity| i.mr_td = [0x12; 48]) as fn(&mut EnclaveIdentity),
        |i| i.compose_hash = vec![0xab; 32],
        |i| i.os_image_hash = vec![0xab; 32],
        |i| i.mr_kms = vec![0xab; 48],
        |i| i.key_provider = b"other".to_vec(),
    ] {
        let mut id = identity();
        mutate(&mut id);
        assert_ne!(base, IdentityPolicy::Require(id).digest());
    }
}

/// Length-prefixing keeps two different identities from hashing the same.
#[test]
fn the_identity_digest_is_unambiguous_across_field_boundaries() {
    let mut a = identity();
    a.compose_hash = b"ab".to_vec();
    a.mr_kms = b"c".to_vec();
    let mut b = identity();
    b.compose_hash = b"a".to_vec();
    b.mr_kms = b"bc".to_vec();
    assert_ne!(
        IdentityPolicy::Require(a).digest(),
        IdentityPolicy::Require(b).digest()
    );
}

/// Whatever mode this checkout is built in, the policy and the digest must agree - a build
/// that claims to pin an enclave while carrying the development digest would be the worst of
/// both worlds.
#[test]
fn the_build_mode_and_the_identity_digest_agree() {
    let policy = build_identity_policy();
    assert_eq!(build_identity_digest(), policy.digest());
    assert_eq!(
        enclave_identity::REQUIRES_SPECIFIC_ENCLAVE,
        policy.requires_specific_enclave()
    );
    if enclave_identity::REQUIRES_SPECIFIC_ENCLAVE {
        assert_ne!(*policy, IdentityPolicy::Any, "a pinned build must not accept any enclave");
    } else {
        assert_eq!(*policy, IdentityPolicy::Any);
    }
}

/// The point of pinning, tested against real hardware output.
///
/// Two genuine TDX quotes: one from the enclave this build was pinned to, one from an
/// unrelated third-party machine (dcap-qvl's sample). A pinned build must accept the first
/// and refuse the second. Without pinning both are accepted, which is precisely the hole
/// `evolve-tee` leaves open - any TDX box, running anything, speaking for the bridge.
mod real_hardware {
    use dcap_qvl::tcb_info::{TcbStatus, TcbStatusWithAdvisory};
    use dcap_qvl::verify::VerifiedReport;
    use tee_attestation::*;

    #[derive(serde::Deserialize)]
    struct QuoteFixture {
        quote: String,
        event_log: String,
    }

    /// Build a report from a genuine quote. The DCAP signature is checked elsewhere; what is
    /// under test here is the identity decision, which operates on a verified report.
    fn report_and_log(json: &str) -> (VerifiedReport, Vec<EventLog>) {
        let f: QuoteFixture = serde_json::from_str(json).unwrap();
        let bytes = hex::decode(f.quote.trim_start_matches("0x")).unwrap();
        let quote = dcap_qvl::quote::Quote::parse(&bytes).expect("real quote must parse");
        let events: Vec<EventLog> = serde_json::from_str(&f.event_log).unwrap_or_default();
        let report = VerifiedReport {
            status: "UpToDate".into(),
            advisory_ids: vec![],
            report: quote.report,
            ppid: vec![],
            qe_status: TcbStatusWithAdvisory {
                status: TcbStatus::UpToDate,
                advisory_ids: vec![],
            },
            platform_status: TcbStatusWithAdvisory {
                status: TcbStatus::UpToDate,
                advisory_ids: vec![],
            },
        };
        (report, events)
    }

    fn ours() -> (VerifiedReport, Vec<EventLog>) {
        report_and_log(include_str!("../testdata/enclave/tee_node_quote.json"))
    }

    fn decide(
        policy: &IdentityPolicy,
        (report, events): &(VerifiedReport, Vec<EventLog>),
    ) -> Result<(), IdentityError> {
        check_enclave_identity(policy, report, events, &replay_event_logs(events))
    }

    #[test]
    fn our_deployed_enclaves_event_log_replays_to_its_signed_rtmrs() {
        let (report, events) = ours();
        let measured = get_measurements(&report.report).expect("tdx");
        assert_eq!(replay_event_logs(&events), measured.rtmrs);
    }

    #[test]
    fn the_build_policy_accepts_the_enclave_it_was_pinned_to() {
        assert_eq!(decide(build_identity_policy(), &ours()), Ok(()));
    }

    /// A genuine TDX quote from someone else's machine. This is the attack pinning exists to
    /// stop, and it is only stopped when `require_enclave = true`.
    #[test]
    fn a_foreign_tdx_enclave_is_refused_by_a_pinned_build() {
        let Some(dir) = crate::sample_dir_for_tests() else {
            eprintln!("dcap-qvl sample not found; skipping");
            return;
        };
        let bytes = std::fs::read(dir.join("tdx_quote")).unwrap();
        let quote = dcap_qvl::quote::Quote::parse(&bytes).expect("sample quote");
        let foreign = VerifiedReport {
            status: "UpToDate".into(),
            advisory_ids: vec![],
            report: quote.report,
            ppid: vec![],
            qe_status: TcbStatusWithAdvisory {
                status: TcbStatus::UpToDate,
                advisory_ids: vec![],
            },
            platform_status: TcbStatusWithAdvisory {
                status: TcbStatus::UpToDate,
                advisory_ids: vec![],
            },
        };
        // Give it our own event log, so only the hardware measurement differs. Even that
        // must be enough.
        let (_, events) = ours();
        let replay = replay_event_logs(&events);
        let verdict = check_enclave_identity(build_identity_policy(), &foreign, &events, &replay);

        if enclave_identity::REQUIRES_SPECIFIC_ENCLAVE {
            assert!(
                matches!(
                    verdict,
                    Err(IdentityError::EventLogNotBoundToQuote)
                        | Err(IdentityError::PlatformMeasurementMismatch)
                ),
                "a pinned build accepted a foreign enclave: {verdict:?}"
            );
        } else {
            // Unpinned still refuses a log the foreign hardware never measured.
            assert_eq!(verdict, Err(IdentityError::EventLogNotBoundToQuote));
        }
    }
}
