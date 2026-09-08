//! Attestation verification: is this a real TDX quote, and does it say what it claims?
//!
//! Three steps, in order, none of which may be skipped:
//!
//! 1. [`verify_quote`] - Intel's signature chain over the quote, against collateral, at a
//!    given time. Answers "did genuine hardware sign this?".
//! 2. [`replay_event_logs`] - fold the dstack event log into RTMRs. Compared against the
//!    quote by [`crate::enclave_identity`], this answers "is this log the one the hardware
//!    measured?".
//! 3. [`check_attested_report`] - the payload actually rides in `report_data`, the
//!    transition is legal, and the prover's clock is anchored to attested chain time.
//!
//! Everything here operates on public data; there are no secrets in a quote.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha384};

use crate::enclave_identity::{check_enclave_identity, IdentityError, IdentityPolicy};
use crate::ism::{decode_attested_update, hash_attested_update, AttestedUpdate, TransitionError};
use dcap_qvl::quote::Report;
use dcap_qvl::verify::VerifiedReport;
use dcap_qvl::QuoteCollateralV3;

/// How far ahead of the attested chain head the prover's clock may claim to be.
///
/// `now` is supplied by an untrusted host and drives every certificate, CRL and TCB validity
/// window. Left free, a host could name a time in the past and revive a platform Intel has
/// since revoked. Anchoring it to the attested head timestamp - which only moves forward
/// across an ISM's state chain - bounds that rewind to this window.
pub const MAX_QUOTE_SKEW_SECS: u64 = 30 * 60;

/// dstack's own runtime event type, outside the TCG-defined range.
pub const DSTACK_RUNTIME_EVENT_TYPE: u32 = 0x0800_0001;
/// dstack records application identity in IMR 3.
pub const APPLICATION_IMR: u32 = 3;
pub const RTMR_COUNT: usize = 4;

// ---------------------------------------------------------------------------
// Quote
// ---------------------------------------------------------------------------

/// Measurements a relying party may pin, in TDX report layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurements {
    pub mr_td: [u8; 48],
    pub rtmrs: [[u8; 48]; RTMR_COUNT],
    pub mr_config_id: [u8; 48],
}

/// Verify a quote against Intel's collateral.
///
/// Pins the RustCrypto backend rather than letting `DefaultConfig` choose, because that is
/// the backend whose `p256` and `sha2` calls SP1's precompile patches intercept - almost all
/// of the proving cost sits here. `QuoteVerifier::new_prod()` also rejects debug-mode TDs,
/// debug SGX enclaves and service TDs, for TD1.0 and TD1.5 alike.
pub fn verify_quote(
    quote: &[u8],
    collateral: &QuoteCollateralV3,
    now_secs: u64,
) -> anyhow::Result<VerifiedReport> {
    dcap_qvl::verify::rustcrypto::verify(quote, collateral, now_secs)
}

/// Match the enum directly. `Report::as_td10()` also returns `Some` for a TD1.5 report, so
/// an `as_td10()`-first chain silently makes every TD1.5 branch dead code - the bug that
/// leaves `evolve-tee`'s `mr_service_td` check unreachable.
pub fn get_report_data(report: &Report) -> [u8; 64] {
    match report {
        Report::SgxEnclave(r) => r.report_data,
        Report::TD10(r) => r.report_data,
        Report::TD15(r) => r.base.report_data,
    }
}

pub fn get_measurements(report: &Report) -> Option<Measurements> {
    let base = match report {
        Report::TD10(r) => r,
        Report::TD15(r) => &r.base,
        Report::SgxEnclave(_) => return None,
    };
    Some(Measurements {
        mr_td: base.mr_td,
        rtmrs: [base.rt_mr0, base.rt_mr1, base.rt_mr2, base.rt_mr3],
        mr_config_id: base.mr_config_id,
    })
}

// ---------------------------------------------------------------------------
// dstack event log
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventLog {
    pub imr: u32,
    pub event_type: u32,
    #[serde(with = "hex_digest")]
    pub digest: [u8; 48],
    pub event: String,
    #[serde(with = "hex_bytes")]
    pub event_payload: Vec<u8>,
}

pub fn sha384(data: &[u8]) -> [u8; 48] {
    Sha384::digest(data).into()
}

/// dstack v1 preimage: `event_type_le || ":" || name || ":" || payload`.
pub fn event_preimage_v1(event_type: u32, event: &str, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(6 + event.len() + payload.len());
    buf.extend_from_slice(&event_type.to_le_bytes());
    buf.push(b':');
    buf.extend_from_slice(event.as_bytes());
    buf.push(b':');
    buf.extend_from_slice(payload);
    buf
}

/// dstack v2 preimage: RFC 8785 canonical JSON of `{name, payload, type}`.
pub fn event_preimage_v2(event_type: u32, event: &str, payload: &[u8]) -> Vec<u8> {
    let mut s = String::from("{\"name\":");
    push_json_string(&mut s, event);
    s.push_str(",\"payload\":");
    push_json_string(&mut s, &hex::encode(payload));
    s.push_str(",\"type\":");
    s.push_str(&event_type.to_string());
    s.push('}');
    s.into_bytes()
}

/// True when `digest` really commits to this entry's `(event, payload)` under either dstack
/// event-log version. Accepting both keeps us correct across image upgrades; each is a
/// collision-resistant commitment to the same pair, so allowing either loses nothing.
pub fn event_digest_matches(e: &EventLog) -> bool {
    let d = e.digest;
    d == sha384(&event_preimage_v1(e.event_type, &e.event, &e.event_payload))
        || d == sha384(&event_preimage_v2(e.event_type, &e.event, &e.event_payload))
}

/// Fold the log into the four runtime measurement registers.
///
/// Entries with an explicit digest contribute it directly; IMR 3 entries that omit one
/// (dstack writes those with an empty digest field) have it recomputed. Entries in IMR 0-2
/// without a digest contribute nothing, matching dstack's own replay.
pub fn replay_event_logs(eventlog: &[EventLog]) -> [[u8; 48]; RTMR_COUNT] {
    let mut rtmrs = [[0u8; 48]; RTMR_COUNT];
    for (idx, mr) in rtmrs.iter_mut().enumerate() {
        for e in eventlog.iter().filter(|e| e.imr as usize == idx) {
            let digest = if e.digest != [0u8; 48] {
                e.digest
            } else if e.imr == APPLICATION_IMR {
                sha384(&event_preimage_v1(e.event_type, &e.event, &e.event_payload))
            } else {
                continue;
            };
            let mut h = Sha384::new();
            h.update(*mr);
            h.update(digest);
            *mr = h.finalize().into();
        }
    }
    rtmrs
}

/// Read a pinned value out of the log, refusing any entry whose digest does not commit to
/// the text being read.
///
/// Without this an attacker keeps every genuine digest - so the RTMR replay still matches -
/// and merely relabels the surrounding text to impersonate a pinned key. A duplicated key
/// never resolves, for the same reason.
pub fn get_event_value<'a>(eventlog: &'a [EventLog], name: &str) -> Option<&'a [u8]> {
    let mut found: Option<&'a EventLog> = None;
    for e in eventlog.iter().filter(|e| e.event == name) {
        if found.is_some() {
            return None;
        }
        found = Some(e);
    }
    let e = found?;
    let self_consistent = if e.digest == [0u8; 48] {
        // No digest recorded, so the replay used our recomputation, which already binds
        // (event, payload) into the RTMR.
        e.imr == APPLICATION_IMR
    } else {
        event_digest_matches(e)
    };
    self_consistent.then_some(e.event_payload.as_slice())
}

// ---------------------------------------------------------------------------
// The whole check
// ---------------------------------------------------------------------------

/// Everything an SP1 program reads. All of it is public.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationInputs {
    pub quote: Vec<u8>,
    /// The dstack event log, as dstack returned it.
    pub event_log: Vec<u8>,
    /// SCALE-encoded [`QuoteCollateralV3`].
    ///
    /// Carried as bytes rather than as the struct because dcap-qvl's serde implementation is
    /// not round-trip safe under bincode - re-decoding what it wrote fails with an
    /// unexpected EOF. SCALE is what the type derives natively and round-trips by
    /// construction, so encoding bugs cannot silently reach the verifier.
    pub collateral: Vec<u8>,
    pub now: u64,
    /// Canonically encoded [`AttestedUpdate`].
    pub payload: Vec<u8>,
}

impl AttestationInputs {
    pub fn encode_collateral(collateral: &QuoteCollateralV3) -> Vec<u8> {
        scale::Encode::encode(collateral)
    }

    pub fn decode_collateral(&self) -> Result<QuoteCollateralV3, AttestationError> {
        scale::Decode::decode(&mut self.collateral.as_slice())
            .map_err(|_| AttestationError::CollateralMalformed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttestationError {
    #[error("quote failed DCAP verification: {0}")]
    QuoteInvalid(String),
    #[error("event log is not valid JSON")]
    EventLogMalformed,
    #[error("collateral is not valid SCALE")]
    CollateralMalformed,
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("attested payload is malformed")]
    PayloadMalformed,
    #[error("report_data does not commit to the payload")]
    PayloadNotAttested,
    #[error("report_data has unaccounted-for bytes past the payload hash")]
    ReportDataNotPadded,
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error("prover clock {now} is behind the attested head {head}")]
    ClockBehindAttestedHead { now: u64, head: u64 },
    #[error("prover clock {now} is more than the allowed skew ahead of the attested head {head}")]
    ClockTooFarAhead { now: u64, head: u64 },
}

/// Verify a quote and everything it must say before its payload may be believed.
pub fn verify_attestation(
    inputs: &AttestationInputs,
    policy: &IdentityPolicy,
) -> Result<AttestedUpdate, AttestationError> {
    let collateral = inputs.decode_collateral()?;
    let report = verify_quote(&inputs.quote, &collateral, inputs.now)
        .map_err(|e| AttestationError::QuoteInvalid(e.to_string()))?;
    check_attested_report(&report, &inputs.event_log, &inputs.payload, inputs.now, policy)
}

/// Everything after the DCAP signature check.
///
/// Split out so the security-critical path is exercised by ordinary tests without needing a
/// genuine signed quote.
pub fn check_attested_report(
    report: &VerifiedReport,
    event_log_json: &[u8],
    payload: &[u8],
    now: u64,
    policy: &IdentityPolicy,
) -> Result<AttestedUpdate, AttestationError> {
    let event_log: Vec<EventLog> =
        serde_json::from_slice(event_log_json).map_err(|_| AttestationError::EventLogMalformed)?;

    let rtmrs = replay_event_logs(&event_log);
    check_enclave_identity(policy, report, &event_log, &rtmrs)?;

    let update = decode_attested_update(payload).map_err(|_| AttestationError::PayloadMalformed)?;

    let report_data = get_report_data(&report.report);
    if report_data[..32] != hash_attested_update(&update) {
        return Err(AttestationError::PayloadNotAttested);
    }
    if report_data[32..] != [0u8; 32] {
        return Err(AttestationError::ReportDataNotPadded);
    }

    crate::ism::check_transition(&update.prev_state, &update.new_state)?;

    let head = update.new_state.timestamp;
    if now < head {
        return Err(AttestationError::ClockBehindAttestedHead { now, head });
    }
    if now > head + MAX_QUOTE_SKEW_SECS {
        return Err(AttestationError::ClockTooFarAhead { now, head });
    }
    Ok(update)
}

fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(s.strip_prefix("0x").unwrap_or(&s)).map_err(serde::de::Error::custom)
    }
}

mod hex_digest {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; 48], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }

    /// dstack writes an empty string when no digest was recorded; that decodes to zeros.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 48], D::Error> {
        let s = String::deserialize(d)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        if s.is_empty() {
            return Ok([0u8; 48]);
        }
        hex::decode(s)
            .map_err(serde::de::Error::custom)?
            .try_into()
            .map_err(|_| serde::de::Error::custom("digest must be 48 bytes"))
    }
}
