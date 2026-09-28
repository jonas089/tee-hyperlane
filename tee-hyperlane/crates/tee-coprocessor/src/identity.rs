//! An enclave's identity, read from its quote and dstack's event log.
//!
//! The quote is signed by the hardware; the event log is not. So nothing is read from the log
//! until it replays to the RTMRs in the quote, and no entry is read unless its digest commits
//! to its own name and payload. Used to pin a new enclave (`tee-hyperlane identity`) and to
//! show the measurements behind a batch in the API.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha384};

/// dstack records the application's identity in RTMR 3.
const APPLICATION_IMR: u32 = 3;

/// The five fields both ISMs pin, as `x/teeism`'s `create` and the ISM scripts read them.
#[derive(Debug, Serialize)]
pub struct Identity {
    pub mr_td: String,
    pub os_image_hash: String,
    pub compose_hash: String,
    pub mr_kms: String,
    pub key_provider: String,
}

impl Identity {
    /// Read from a quote and its event log, refusing a log the quote did not measure.
    pub fn from_quote(quote: &str, event_log: &str) -> Result<Self> {
        let quote = dcap_qvl::quote::Quote::parse(&hex::decode(quote.trim_start_matches("0x"))?)
            .map_err(|e| anyhow::anyhow!("quote does not parse: {e:?}"))?;
        let td = quote.report.as_td10().context("not a TDX quote")?;
        let log: Vec<EventLog> = serde_json::from_str(event_log).context("event log")?;
        anyhow::ensure!(
            replay(&log) == [td.rt_mr0, td.rt_mr1, td.rt_mr2, td.rt_mr3],
            "event log does not replay to the quote's RTMRs"
        );
        let field = |name: &str| -> Result<String> {
            value(&log, name).map(hex::encode).with_context(|| {
                format!("event `{name}` missing, duplicated or not self-consistent")
            })
        };
        Ok(Identity {
            mr_td: hex::encode(td.mr_td),
            os_image_hash: field("os-image-hash")?,
            compose_hash: field("compose-hash")?,
            mr_kms: field("mr-kms")?,
            key_provider: field("key-provider")?,
        })
    }

    /// Read a live enclave's `/identity`.
    pub async fn fetch(url: &str) -> Result<Self> {
        let body: serde_json::Value =
            reqwest::get(format!("{}/identity", url.trim_end_matches('/')))
                .await?
                .error_for_status()?
                .json()
                .await?;
        Self::from_quote(
            body["quote"].as_str().context("no quote in /identity")?,
            body["event_log"]
                .as_str()
                .context("no event log in /identity")?,
        )
    }
}

/// One entry of dstack's event log, as the enclave reports it.
#[derive(Clone, Debug, Deserialize)]
struct EventLog {
    imr: u32,
    event_type: u32,
    #[serde(deserialize_with = "digest")]
    digest: [u8; 48],
    event: String,
    #[serde(deserialize_with = "bytes")]
    event_payload: Vec<u8>,
}

/// Fold the log into RTMR 0..3. IMR 3 entries without a digest have it recomputed, as dstack's
/// own replay does; entries in IMR 0..2 without one contribute nothing.
fn replay(log: &[EventLog]) -> [[u8; 48]; 4] {
    let mut rtmrs = [[0u8; 48]; 4];
    for (i, mr) in rtmrs.iter_mut().enumerate() {
        for e in log.iter().filter(|e| e.imr as usize == i) {
            let d = if e.digest != [0u8; 48] {
                e.digest
            } else if e.imr == APPLICATION_IMR {
                sha384(&preimage_v1(e))
            } else {
                continue;
            };
            *mr = Sha384::new()
                .chain_update(*mr)
                .chain_update(d)
                .finalize()
                .into();
        }
    }
    rtmrs
}

/// A named value, only if exactly one entry carries the name and its digest commits to its
/// text. Otherwise an attacker keeps every genuine digest, so the replay still matches, and
/// relabels the text around one to impersonate a pinned field.
fn value<'a>(log: &'a [EventLog], name: &str) -> Option<&'a [u8]> {
    let mut named = log.iter().filter(|e| e.event == name);
    let e = named.next()?;
    if named.next().is_some() {
        return None;
    }
    let committed = if e.digest == [0u8; 48] {
        e.imr == APPLICATION_IMR // the replay recomputed it, which binds the text already
    } else {
        e.digest == sha384(&preimage_v1(e)) || e.digest == sha384(&preimage_v2(e))
    };
    committed.then_some(e.event_payload.as_slice())
}

/// dstack v1: `event_type_le ":" name ":" payload`.
fn preimage_v1(e: &EventLog) -> Vec<u8> {
    [
        &e.event_type.to_le_bytes()[..],
        b":",
        e.event.as_bytes(),
        b":",
        &e.event_payload,
    ]
    .concat()
}

/// dstack v2: canonical JSON of `{name, payload, type}`, keys sorted.
fn preimage_v2(e: &EventLog) -> Vec<u8> {
    serde_json::json!({ "name": e.event, "payload": hex::encode(&e.event_payload), "type": e.event_type })
        .to_string()
        .into_bytes()
}

fn sha384(data: &[u8]) -> [u8; 48] {
    Sha384::digest(data).into()
}

fn bytes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(d)?;
    hex::decode(s.trim_start_matches("0x")).map_err(serde::de::Error::custom)
}

/// dstack writes an empty string when it recorded no digest; that reads as zeros.
fn digest<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u8; 48], D::Error> {
    let b = bytes(d)?;
    if b.is_empty() {
        return Ok([0u8; 48]);
    }
    b.try_into()
        .map_err(|_| serde::de::Error::custom("digest must be 48 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ET: u32 = 0x0800_0001;

    fn ev(name: &str, payload: &[u8]) -> EventLog {
        let mut e = EventLog {
            imr: APPLICATION_IMR,
            event_type: ET,
            digest: [0; 48],
            event: name.into(),
            event_payload: payload.to_vec(),
        };
        e.digest = sha384(&preimage_v1(&e));
        e
    }

    fn log() -> Vec<EventLog> {
        vec![
            ev("os-image-hash", &[0xbb; 32]),
            ev("compose-hash", &[0xaa; 32]),
            ev("instance-id", b"a"),
        ]
    }

    /// A real `/identity` response from one of our CVMs, which must read as it did under
    /// `circuit-tool identity`.
    #[test]
    fn a_live_enclave_reads_back() {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/identity.json")).unwrap();
        let id = Identity::from_quote(
            v["quote"].as_str().unwrap(),
            v["event_log"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(id.mr_td.len(), 96);
        assert_eq!(id.compose_hash.len(), 64);
    }

    #[test]
    fn a_log_the_quote_did_not_measure_is_refused() {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/identity.json")).unwrap();
        let mut log: Vec<serde_json::Value> =
            serde_json::from_str(v["event_log"].as_str().unwrap()).unwrap();
        log.push(
            serde_json::json!({ "imr": 3, "event_type": ET, "digest": "",
                                     "event": "extra", "event_payload": "00" }),
        );
        let err = Identity::from_quote(
            v["quote"].as_str().unwrap(),
            &serde_json::to_string(&log).unwrap(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not replay"), "{err}");
    }

    #[test]
    fn relabelled_text_is_refused_although_the_rtmrs_still_match() {
        let good = log();
        let mut forged = good.clone();
        forged[2].event = "compose-hash".into();
        forged[2].event_payload = vec![0xde; 32];
        forged.remove(1);
        assert_eq!(value(&forged, "compose-hash"), None);
        assert_eq!(value(&good, "compose-hash"), Some(&[0xaa; 32][..]));
    }

    #[test]
    fn a_digest_that_does_not_commit_to_its_payload_is_refused() {
        let mut l = log();
        l[1].event_payload = vec![0xde; 32];
        assert_eq!(value(&l, "compose-hash"), None);
    }

    #[test]
    fn a_duplicated_name_never_resolves() {
        let mut l = log();
        l.push(ev("compose-hash", &[0xde; 32]));
        assert_eq!(value(&l, "compose-hash"), None);
    }

    #[test]
    fn both_dstack_digest_versions_are_accepted() {
        let mut e = ev("app-id", &[0xde, 0xad]);
        assert!(value(std::slice::from_ref(&e), "app-id").is_some(), "v1");
        e.digest = sha384(&preimage_v2(&e));
        assert!(value(std::slice::from_ref(&e), "app-id").is_some(), "v2");
        assert_eq!(
            String::from_utf8(preimage_v2(&e)).unwrap(),
            r#"{"name":"app-id","payload":"dead","type":134217729}"#
        );
    }
}
