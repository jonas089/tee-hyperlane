//! The relayer: one loop per route, the same for every chain.
//!
//! Each pass does the whole pipeline or nothing:
//!
//! 1. finish a batch left staged by an earlier pass, if there is one
//! 2. read the ISM's trusted state from the destination
//! 3. gather the origin's proofs for its newest head; stop if the tree has not grown
//! 4. index the messages in between, and check they are exactly the new leaves
//! 5. stop unless one of them is ours, or the heartbeat is due
//! 6. ask the enclave to attest, stage the result, and submit it
//!
//! Every pass also retries the messages parked in `undelivered/`: ones the destination refused,
//! and ones left over from a batch whose attestation landed but whose delivery could not finish.
//! A message the ISM covers is never forgotten, only parked, and a parked message is reported
//! by the API until it lands.
//!
//! The ISM's state on chain is the only progress marker, so restarting is the same as
//! continuing. A batch is staged on disk only so a crash between attesting and submitting does
//! not lose it, and it is always finished before a new one starts: abandoning it after the ISM
//! advanced past its start would put its leaves behind every later snapshot for good.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::config::{self, Config};
use crate::destination::{Batch, Delivery, Destination, Outcome, Stale};
use crate::origin::{Indexer, Message};

/// A route that has not advanced in this long attests anyway, if its origin has any new leaf.
/// Quiet routes otherwise fall far behind, and a route far behind has to read further back than
/// its RPCs will serve: four days of that stopped every Celestia route once.
const HEARTBEAT: Duration = Duration::from_secs(12 * 60 * 60);
/// A staged batch older than this is rebuilt rather than retried. Both kinds of ISM refuse an
/// attestation older than 24h, so past that it could never land, and retrying it forever would
/// stall the route long after whatever delayed it had passed.
const STAGED_BATCH_MAX_AGE: Duration = Duration::from_secs(23 * 60 * 60);
/// Retries back off by doubling up to this ceiling, so a route broken overnight still notices
/// within the hour once it is fixed.
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// A parked message is retried after this, doubling per attempt up to `MAX_REDELIVERY_WAIT`.
const REDELIVERY_WAIT: u64 = 60;
const MAX_REDELIVERY_WAIT: u64 = 6 * 60 * 60;

/// A message the ISM covers that has not been delivered, kept in `undelivered/<id>.json` until
/// it is. Read by the tracker and the API, written only here.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct Parked {
    pub id: String,
    /// The message bytes, hex.
    pub message: String,
    pub attempts: u32,
    pub first_failed_at: u64,
    pub last_attempt_at: u64,
    pub next_attempt_at: u64,
    pub reason: String,
    /// The last failed transaction, if one landed.
    pub tx: Option<String>,
}

/// A delivery this relayer made or saw, kept in `delivered/<id>.json`.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct Delivered {
    pub id: String,
    /// Our transaction, or none when the message was already processed by someone else.
    pub tx: Option<String>,
    pub at: u64,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn redelivery_wait(attempts: u32) -> u64 {
    REDELIVERY_WAIT
        .saturating_mul(1 << attempts.min(16))
        .min(MAX_REDELIVERY_WAIT)
}

pub struct Route {
    config: config::Route,
    origin: Box<dyn Indexer>,
    destination: Box<dyn Destination>,
    destination_domain: u32,
    enclave: EnclaveClient,
    dir: PathBuf,
}

impl Route {
    pub fn new(config: &Config, route: &config::Route) -> Result<Self> {
        let dir = config.proof_dir().join(&route.name);
        std::fs::create_dir_all(dir.join("staging"))?;
        std::fs::create_dir_all(dir.join("undelivered"))?;
        std::fs::create_dir_all(dir.join("delivered"))?;
        Ok(Self {
            origin: config
                .indexer(&route.from)
                .with_context(|| format!("route {}", route.name))?,
            destination: config
                .destination(&route.to, &route.ism)
                .with_context(|| format!("route {}", route.name))?,
            destination_domain: config.domain(&route.to)?,
            enclave: EnclaveClient::new(&route.enclave),
            config: route.clone(),
            dir,
        })
    }

    /// Run forever: a pass every tick while healthy, backing off while failing.
    pub async fn run(self, tick: Duration) {
        let mut failures: u32 = 0;
        loop {
            match self.pass().await {
                Ok(Some(height)) => {
                    failures = 0;
                    self.clear_blocker();
                    info!(route = %self.config.name, height, "batch delivered");
                    // Straight round again: the origin may already have more.
                    continue;
                }
                Ok(None) => {
                    failures = 0;
                    self.clear_blocker();
                }
                Err(e) => {
                    failures = failures.saturating_add(1);
                    let wait = backoff(tick, failures);
                    warn!(route = %self.config.name, error = %format!("{e:#}"), consecutive_failures = failures, retry_in_secs = wait.as_secs(), "route failed");
                    self.record_blocker(&format!("{e:#}"), failures);
                }
            }
            tokio::time::sleep(backoff(tick, failures)).await;
        }
    }

    /// One pass. `Some(height)` when a batch was delivered.
    async fn pass(&self) -> Result<Option<u64>> {
        self.redeliver().await;
        if let Some(height) = self.submit_staged().await? {
            return Ok(Some(height));
        }

        let trusted = self
            .destination
            .state()
            .await
            .context("reading the ISM's trusted state")?;
        let step = self.origin.gather(&trusted).await?;
        self.record_head(step.head);
        if step.leaves.is_empty() {
            debug!(route = %self.config.name, head = step.head, "idle");
            return Ok(None);
        }

        let messages = self.origin.index(trusted.height, step.head).await?;
        check_index(step.leaves.len(), messages.len())?;
        if !worth_attesting(&messages, self.destination_domain, &self.config.routers)
            && !self.heartbeat_due()
        {
            debug!(route = %self.config.name, leaves = messages.len(), "nothing for our routes");
            return Ok(None);
        }

        info!(route = %self.config.name, from = trusted.height, to = step.head, leaves = messages.len(), "attesting");
        let request = json!({
            "protocol": tee_node::attest::PROTOCOL_VERSION,
            "trusted_state": hex::encode(trusted.encode()),
            "chain": step.chain,
            "input": step.input,
            "tree": step.tree,
            "tree_snapshot": step.tree_snapshot,
            "message_ids": messages.iter().map(|m| m.id).collect::<Vec<_>>(),
            "merkle_tree_address": step.tree_address,
        });
        let attestation = self.enclave.attest(&request).await?;
        let record = json!({
            "attestation": {
                "quote": attestation.quote,
                "event_log": attestation.event_log,
                "payload": attestation.payload,
                "new_state": attestation.new_state,
            },
            "messages": messages.iter().map(|m| hex::encode(&m.bytes)).collect::<Vec<_>>(),
        });
        std::fs::write(self.staged(), serde_json::to_vec_pretty(&record)?)?;
        let _ = std::fs::write(self.dir.join("advanced"), b"");
        self.submit_staged().await
    }

    fn staged(&self) -> PathBuf {
        self.dir.join("staging").join("batch.json")
    }

    /// Submit the staged batch, if any, and file it where the API serves it.
    async fn submit_staged(&self) -> Result<Option<u64>> {
        let path = self.staged();
        let Ok(raw) = std::fs::read(&path) else {
            return Ok(None);
        };
        let age = std::fs::metadata(&path)?
            .modified()?
            .elapsed()
            .unwrap_or_default();
        let mut record: Value = serde_json::from_slice(&raw)?;
        let att = &record["attestation"];
        let text = |v: &Value| {
            v.as_str()
                .map(str::to_string)
                .context("malformed staged batch")
        };
        let batch = Batch {
            quote: text(&att["quote"])?,
            event_log: text(&att["event_log"])?,
            payload: hex::decode(text(&att["payload"])?.trim_start_matches("0x"))?,
            messages: record["messages"]
                .as_array()
                .context("messages")?
                .iter()
                .filter_map(|m| hex::decode(m.as_str()?).ok())
                .collect(),
        };
        if age > STAGED_BATCH_MAX_AGE {
            warn!(route = %self.config.name, age_secs = age.as_secs(), "discarding a staged batch too old to be accepted; rebuilding");
            self.discard(&batch, &path).await?;
            return Ok(None);
        }
        let deliveries = match self.destination.submit(&batch).await {
            Ok(d) => d,
            // The ISM moved past this batch's start, so no retry can land it. The next pass
            // builds from where the ISM actually is, which skips nothing.
            Err(e) if e.is::<Stale>() => {
                warn!(route = %self.config.name, "discarding a batch the ISM has moved past; rebuilding");
                self.discard(&batch, &path).await?;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        for d in deliveries {
            self.settle(d);
        }

        let update = tee_node::state::AttestedUpdate::decode(&batch.payload)?;
        let height = update.new_state.height;
        record["height"] = height.into();
        record["state_root"] = format!("0x{}", hex::encode(update.new_state.state_root)).into();
        record["quote"] = batch.quote.clone().into();
        record["measurements"] =
            serde_json::to_value(crate::api::measurements(&batch.quote, &batch.event_log)?)?;
        record["batch"] = update
            .message_ids
            .iter()
            .map(|id| format!("0x{}", hex::encode(id)))
            .collect::<Vec<_>>()
            .into();
        std::fs::write(
            self.dir.join(format!("{height}.json")),
            serde_json::to_vec_pretty(&record)?,
        )?;
        std::fs::remove_file(&path)?;
        Ok(Some(height))
    }

    /// Drop a staged batch that can no longer land. If the ISM already covers it, its
    /// attestation did land, and a rebuild would start after its messages: park the ones not
    /// yet delivered so they are not lost.
    async fn discard(&self, batch: &Batch, path: &std::path::Path) -> Result<()> {
        let covered = tee_node::state::IsmState::decode(batch.new_state())?.height;
        let ism = self
            .destination
            .state()
            .await
            .context("reading the ISM before discarding a batch")?;
        if ism.height >= covered {
            for (id, message) in batch.for_domain(self.destination_domain) {
                if !self.destination.delivered(&id).await? {
                    warn!(route = %self.config.name, id, "parking a covered message from a discarded batch");
                    self.park(
                        &id,
                        message,
                        "left over from a batch that could not finish".into(),
                        None,
                    );
                }
            }
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    /// Record what became of one message.
    fn settle(&self, d: Delivery) {
        match d.outcome {
            Outcome::Delivered { tx } => self.mark_delivered(&d.id, Some(tx)),
            Outcome::AlreadyDelivered => self.mark_delivered(&d.id, None),
            Outcome::Refused { tx, reason } => {
                warn!(route = %self.config.name, id = d.id, reason, "delivery refused; parked for redelivery");
                self.park(&d.id, &d.message, reason, tx);
            }
        }
    }

    fn mark_delivered(&self, id: &str, tx: Option<String>) {
        let path = self.dir.join("delivered").join(format!("{id}.json"));
        // Our own transaction wins over "someone else did it", which a retry would report.
        if tx.is_none() && path.exists() {
            return;
        }
        let body = Delivered {
            id: id.to_string(),
            tx,
            at: now(),
        };
        if let Err(e) = write_json(&path, &body) {
            warn!(route = %self.config.name, id, error = %e, "could not record a delivery");
        }
        let _ = std::fs::remove_file(self.dir.join("undelivered").join(format!("{id}.json")));
    }

    fn park(&self, id: &str, message: &[u8], reason: String, tx: Option<String>) {
        let path = self.dir.join("undelivered").join(format!("{id}.json"));
        let at = now();
        let previous: Option<Parked> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let attempts = previous.as_ref().map_or(0, |p| p.attempts) + 1;
        let body = Parked {
            id: id.to_string(),
            message: hex::encode(message),
            attempts,
            first_failed_at: previous.as_ref().map_or(at, |p| p.first_failed_at),
            last_attempt_at: at,
            next_attempt_at: at + redelivery_wait(attempts - 1),
            reason,
            tx,
        };
        if let Err(e) = write_json(&path, &body) {
            warn!(route = %self.config.name, id, error = %e, "could not park a message");
        }
    }

    /// Retry every parked message that is due. Never fails the pass: a message that still
    /// cannot land stays parked, and stays reported.
    async fn redeliver(&self) {
        let Ok(entries) = std::fs::read_dir(self.dir.join("undelivered")) else {
            return;
        };
        for entry in entries.flatten() {
            let Some(parked) = std::fs::read(entry.path())
                .ok()
                .and_then(|b| serde_json::from_slice::<Parked>(&b).ok())
            else {
                continue;
            };
            if parked.next_attempt_at > now() {
                continue;
            }
            let Ok(message) = hex::decode(&parked.message) else {
                continue;
            };
            match self.destination.deliver(&message).await {
                Ok(outcome) => self.settle(Delivery {
                    id: parked.id,
                    message,
                    outcome,
                }),
                Err(e) => {
                    warn!(route = %self.config.name, id = parked.id, error = %format!("{e:#}"), "redelivery failed");
                    self.park(&parked.id, &message, format!("{e:#}"), parked.tx);
                }
            }
        }
    }

    fn heartbeat_due(&self) -> bool {
        heartbeat_due(&self.dir)
    }

    /// How far the origin could go, for the dashboard; written even when idle, which is exactly
    /// when it is worth seeing.
    fn record_head(&self, head: u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = std::fs::write(
            self.dir.join("head.json"),
            json!({ "target": head, "updated_at": now }).to_string(),
        );
    }

    /// Why the route last failed, for the dashboard: a batch stuck at submission looks like one
    /// still attesting from outside, and the error is the difference.
    fn record_blocker(&self, error: &str, failures: u32) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // When this run of failures began, carried over from the last write, so the monitor can
        // tell a blip from an outage.
        let path = self.dir.join("blocked.json");
        let since = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|b| b["since"].as_u64())
            .filter(|_| failures > 1)
            .unwrap_or(now);
        let body =
            json!({ "error": error, "consecutiveFailures": failures, "at": now, "since": since });
        let _ = std::fs::write(path, body.to_string());
    }

    fn clear_blocker(&self) {
        let _ = std::fs::remove_file(self.dir.join("blocked.json"));
    }
}

/// Start every route, the tracker and the API, and run until one of them stops.
pub async fn serve(config: Config) -> Result<()> {
    let tick = Duration::from_secs(config.tick_secs);
    let mut tasks = tokio::task::JoinSet::new();
    for route in &config.routes {
        info!(route = %route.name, from = %route.from, to = %route.to, "configured");
        tasks.spawn(Route::new(&config, route)?.run(tick));
    }
    let tracker = crate::tracker::Tracker::new(&config)?;
    tracker.spawn(&mut tasks);
    let api = crate::api::Api::new(&config)?;
    let listen = config.api_listen.clone();
    tasks.spawn(async move {
        if let Err(e) = crate::api::serve(api, tracker, &listen).await {
            warn!(error = %e, "api stopped");
        }
    });
    // A route, a watcher and the monitor each run until the process stops. If anything ends, take the service down so systemd
    // restarts it, rather than leaving a direction silently dead.
    tasks.join_next().await;
    anyhow::bail!("a route or the api stopped")
}

fn write_json(path: &std::path::Path, value: &impl serde::Serialize) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// The tree says how many leaves the range must hold, so a scan that disagrees is caught here,
/// where the cause can be named, rather than by the enclave's replay a round trip later.
fn check_index(expected: usize, found: usize) -> Result<()> {
    anyhow::ensure!(
        found == expected,
        "the tree grew by {expected} leaves but the index found {found}; {}",
        if found < expected {
            "the origin endpoint is not reporting all of them, which a pruned endpoint does by answering an \
             old range with an empty result instead of an error; point `logs_rpc` or `archive_rpc` at one \
             that keeps that range"
        } else {
            "the index is counting leaves from another tree on this chain"
        }
    );
    Ok(())
}

/// Is any of these messages for one of our routers on the destination? With no routers
/// configured, any message for the destination counts. An origin's tree is shared by every
/// bridge on it, so without this every transfer anywhere starts an attestation here.
fn worth_attesting(messages: &[Message], destination: u32, routers: &[String]) -> bool {
    let ours: Vec<[u8; 32]> = routers
        .iter()
        .filter_map(|r| {
            let raw = hex::decode(r.trim_start_matches("0x")).ok()?;
            let mut padded = [0u8; 32];
            padded[32 - raw.len().min(32)..].copy_from_slice(&raw[raw.len().saturating_sub(32)..]);
            Some(padded)
        })
        .collect();
    messages.iter().any(|m| {
        hyperlane_types::decode_hyperlane_message(&m.bytes).is_ok_and(|d| {
            d.destination == destination && (ours.is_empty() || ours.contains(&d.recipient))
        })
    })
}

fn heartbeat_due(dir: &std::path::Path) -> bool {
    match std::fs::metadata(dir.join("advanced")).and_then(|m| m.modified()) {
        Ok(at) => at.elapsed().is_ok_and(|d| d > HEARTBEAT),
        // Never advanced under this binary: take the heartbeat rather than wait twelve hours to
        // find out the route is stuck.
        Err(_) => true,
    }
}

fn backoff(tick: Duration, failures: u32) -> Duration {
    if failures == 0 {
        return tick;
    }
    tick.saturating_mul(1 << failures.min(8))
        .min(MAX_BACKOFF.max(tick))
}

/// The enclave's `/attest` endpoint. Everything sent is public and re-verified inside, so
/// this client needs no trust of its own; a 400 is the enclave refusing what we gathered, and
/// its message says which check failed.
struct EnclaveClient {
    url: String,
    http: reqwest::Client,
}

/// What the enclave returns once it has verified and signed.
#[derive(Debug, Clone, Deserialize)]
struct Attestation {
    /// Hex TDX quote over sha256(payload).
    pub quote: String,
    /// dstack runtime event log, JSON text.
    pub event_log: String,
    /// Canonical attested payload, hex.
    pub payload: String,
    /// The ISM state this update moves to, hex.
    pub new_state: String,
}

#[derive(Debug, Deserialize)]
struct EnclaveError {
    error: String,
}

impl EnclaveClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("http client"),
        }
    }

    /// `request` is `tee_node::attest::AttestRequest`, serialised.
    pub async fn attest(&self, request: &serde_json::Value) -> Result<Attestation> {
        let response = self
            .http
            .post(format!("{}/attest", self.url))
            .json(request)
            .send()
            .await
            .context("enclave unreachable")?;

        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            // A 400 means the enclave refused what we gathered - that is the enclave doing
            // its job, and the message says which check failed.
            let reason = serde_json::from_str::<EnclaveError>(&body)
                .map(|e| e.error)
                .unwrap_or(body);
            anyhow::bail!("enclave rejected the update ({status}): {reason}");
        }
        Ok(serde_json::from_str(&body).context("enclave response")?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperlane_types::{encode_hyperlane_message, HyperlaneMessage};

    fn message(destination: u32, recipient: [u8; 32]) -> Message {
        let bytes = encode_hyperlane_message(&HyperlaneMessage {
            version: 3,
            nonce: 0,
            origin: 1,
            sender: [0; 32],
            destination,
            recipient,
            body: vec![],
        });
        Message { id: [0; 32], bytes }
    }

    #[test]
    fn only_a_message_for_our_router_is_worth_attesting() {
        let ours = [7u8; 32];
        let routers = vec![format!("0x{}", hex::encode(ours))];
        assert!(worth_attesting(
            &[message(5, [1; 32]), message(5, ours)],
            5,
            &routers
        ));
        assert!(
            !worth_attesting(&[message(5, [1; 32])], 5, &routers),
            "someone else's router"
        );
        assert!(
            !worth_attesting(&[message(6, ours)], 5, &routers),
            "our router, another chain"
        );
        assert!(!worth_attesting(&[], 5, &routers));
    }

    #[test]
    fn with_no_routers_any_message_for_the_destination_counts() {
        assert!(worth_attesting(&[message(5, [1; 32])], 5, &[]));
        assert!(!worth_attesting(&[message(6, [1; 32])], 5, &[]));
    }

    /// An EVM router is 20 bytes; Hyperlane left-pads it to 32.
    #[test]
    fn a_twenty_byte_router_is_compared_left_padded() {
        let mut padded = [0u8; 32];
        padded[12..].copy_from_slice(&[9u8; 20]);
        assert!(worth_attesting(
            &[message(5, padded)],
            5,
            &[format!("0x{}", hex::encode([9u8; 20]))]
        ));
    }

    #[test]
    fn an_index_that_disagrees_with_the_tree_is_refused() {
        assert!(check_index(3, 3).is_ok());
        assert!(check_index(3, 1)
            .unwrap_err()
            .to_string()
            .contains("pruned"));
        assert!(check_index(3, 4)
            .unwrap_err()
            .to_string()
            .contains("another tree"));
    }

    #[test]
    fn the_heartbeat_is_due_until_the_route_advances() {
        let dir = tempfile::tempdir().unwrap();
        assert!(heartbeat_due(dir.path()), "never advanced");
        std::fs::write(dir.path().join("advanced"), b"").unwrap();
        assert!(!heartbeat_due(dir.path()), "just advanced");
        let old = std::time::SystemTime::now() - HEARTBEAT - Duration::from_secs(60);
        filetime::set_file_mtime(
            dir.path().join("advanced"),
            filetime::FileTime::from_system_time(old),
        )
        .unwrap();
        assert!(heartbeat_due(dir.path()), "quiet for over twelve hours");
    }

    #[test]
    fn backoff_grows_then_stops_and_never_undercuts_the_tick() {
        let tick = Duration::from_secs(120);
        assert_eq!(backoff(tick, 0), tick);
        assert_eq!(backoff(tick, 1), Duration::from_secs(240));
        assert_eq!(backoff(tick, 20), MAX_BACKOFF);
        let slow = Duration::from_secs(45 * 60);
        assert_eq!(backoff(slow, 5), slow);
        assert!(
            STAGED_BATCH_MAX_AGE < Duration::from_secs(24 * 60 * 60),
            "must expire before the ISMs refuse it"
        );
        assert!(
            STAGED_BATCH_MAX_AGE > MAX_BACKOFF,
            "must outlive the longest retry pause"
        );
    }
}
