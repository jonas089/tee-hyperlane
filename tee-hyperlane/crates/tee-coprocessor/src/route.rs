//! The relay loop: one per route, attesting new messages and delivering them.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use hyperlane_types::MerkleTree;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::config::{self, Config};
use crate::destination::{Batch, Delivery, Destination, Outcome, Stale};
use crate::origin::{Indexer, Message};

/// A route whose trusted state is this old attests anyway, with no messages if there are none.
/// Quiet routes otherwise fall far behind, and a route far behind has to read further back than
/// its RPCs will serve: four days of that stopped every Celestia route once. Far more than the
/// interval both ISMs require between attestations that leave the tree root where it is
/// (`EMPTY_BATCH_MIN_INTERVAL_SECS`), so the origin's clock always clears it.
pub const HEARTBEAT: Duration = Duration::from_secs(24 * 60 * 60);
/// How far an attestation that carries no new leaves must move the origin's clock. Enforced by
/// `TeeDcapIsm` and `x/teeism`, which hold the previous root; the enclave does not, so this is
/// the relayer's knowledge of the rule rather than a check it can run.
pub const EMPTY_BATCH_MIN_INTERVAL_SECS: u64 = 30 * 60;
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
    /// What proves it against an attested root, hex: `root(32) index(4) proof(32 * 32)`. Absent
    /// in records written before roots were attested; those cannot be delivered any more.
    #[serde(default)]
    pub metadata: String,
    /// The tree the proof was built from, so it can be rebuilt against a later root if the ISM
    /// never attested this one. Absent in records from before it was kept.
    #[serde(default)]
    pub provenance: Option<Provenance>,
    pub attempts: u32,
    pub first_failed_at: u64,
    pub last_attempt_at: u64,
    pub next_attempt_at: u64,
    pub reason: String,
    /// The last failed transaction, if one landed.
    pub tx: Option<String>,
}

/// Where a message's proof came from: the origin tree as of the ISM's trusted height when the
/// step was built, and that height. A proof only lands against the root it was built for, and
/// the ISM may never attest that root (someone else moved it first, or it was replaced), so the
/// relayer keeps enough to prove the message again against whatever root the ISM holds: this
/// frontier plus the leaves indexed from `from_height` to the ISM's height.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct Provenance {
    pub from_height: u64,
    pub frontier: MerkleTree,
}

/// How far up the origin this relayer has accounted for every leaf, kept in `covered.json`: the
/// height, and the tree as of it. The ISM's height is not that marker. Anyone can ask the
/// enclave for an attestation and submit it, so the ISM can move past messages this relayer
/// never indexed; a relayer that only ever looked forward from the ISM's height would leave
/// them attested and undelivered for good. Whenever the ISM is found above this height, the
/// leaves in between are indexed and every message of ours among them is delivered.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
struct Covered {
    height: u64,
    frontier: MerkleTree,
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
    enclaves: EnclavePool,
    dir: PathBuf,
}

impl Route {
    pub fn new(config: &Config, route: &config::Route) -> Result<Self> {
        let dir = config.proof_dir().join(&route.name);
        std::fs::create_dir_all(dir.join("staging"))?;
        std::fs::create_dir_all(dir.join("undelivered"))?;
        std::fs::create_dir_all(dir.join("delivered"))?;
        anyhow::ensure!(
            !route.enclaves.is_empty(),
            "route {} names no enclaves",
            route.name
        );
        Ok(Self {
            origin: config
                .indexer(&route.from)
                .with_context(|| format!("route {}", route.name))?,
            destination: config
                .destination(&route.to, &route.ism)
                .with_context(|| format!("route {}", route.name))?,
            destination_domain: config.domain(&route.to)?,
            enclaves: EnclavePool::new(&route.enclaves, dir.join("enclaves.json")),
            config: route.clone(),
            dir,
        })
    }

    /// Run forever: a pass every tick while healthy, backing off while failing.
    pub async fn run(self, tick: Duration) {
        let mut streak = crate::Streak::default();
        loop {
            let result = self.pass().await;
            if result.is_ok() {
                self.clear_blocker();
                if let Some((failures, lasted)) = streak.ok() {
                    info!(route = %self.config.name, failures, after = %crate::monitor::duration(lasted.as_secs()), "route recovered");
                }
            }
            match result {
                Ok(Some(height)) => {
                    debug!(route = %self.config.name, height, "batch delivered");
                    // Straight round again: the origin may already have more.
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    let error = format!("{e:#}");
                    if let Some(n) = streak.fail(&error) {
                        let wait = backoff(tick, streak.failures());
                        if n == 1 {
                            warn!(route = %self.config.name, error = %error, retry_in_secs = wait.as_secs(), "route failing");
                        } else {
                            warn!(route = %self.config.name, error = %error, consecutive_failures = n, retry_in_secs = wait.as_secs(), "route still failing");
                        }
                    }
                    self.record_blocker(&error, streak.failures());
                }
            }
            tokio::time::sleep(backoff(tick, streak.failures())).await;
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
        // Never fails the pass: a gap that cannot be read yet is read again next time, and the
        // route goes on attesting what is new meanwhile. It is recorded instead, where the
        // monitor and the dashboard read it, because the pass itself will report success.
        match self.reconcile(&trusted).await {
            Ok(()) => self.clear_uncovered(),
            Err(e) => self.record_uncovered(&format!("{e:#}"), trusted.height),
        }
        let step = self.origin.gather(&trusted).await?;
        self.record_head(step.head);
        // An idle step carries no proofs: the origin has not moved past the trusted height.
        if step.chain.is_empty() {
            debug!(route = %self.config.name, head = step.head, "idle");
            self.record_activity(
                "idle",
                format!(
                    "the origin has not moved past the ISM's height {}",
                    trusted.height
                ),
            );
            return Ok(None);
        }
        // The first pass of a route has nothing to reconcile against. Start from here: the tree
        // as of the ISM's height, proven under the root the ISM holds.
        if self.covered().is_none() {
            match trusted_frontier(&step, &trusted) {
                Ok(frontier) => self.set_covered(trusted.height, &frontier),
                Err(e) => {
                    warn!(route = %self.config.name, error = %format!("{e:#}"), "could not read the tree at the ISM's height")
                }
            }
        }
        // Both ISMs refuse a transition that leaves the state root where it is, and the enclave
        // does not check that, so it would attest one anyway. An origin can sit on one root for
        // many blocks (Eden's empty blocks change nothing), so wait for the root to move rather
        // than stage a batch no retry can land.
        if step.state_root.0 == trusted.state_root {
            debug!(route = %self.config.name, head = step.head, "state root unchanged");
            self.record_activity(
                "waiting",
                format!(
                    "the origin's state root at height {} is the one the ISM holds from height {}",
                    step.head, trusted.height
                ),
            );
            return Ok(None);
        }
        let heartbeat = heartbeat_due(trusted.timestamp, now());
        if step.leaves.is_empty() && !heartbeat {
            debug!(route = %self.config.name, head = step.head, "nothing dispatched");
            self.record_activity(
                "waiting",
                format!(
                    "nothing dispatched between heights {} and {}",
                    trusted.height, step.head
                ),
            );
            return Ok(None);
        }

        let messages = if step.leaves.is_empty() {
            Vec::new()
        } else {
            let messages = self.origin.index(trusted.height, step.head).await?;
            check_index(step.leaves.len(), messages.len())?;
            messages
        };
        // Also advance when the trusted height falls too far behind, so the next proof at it
        // stays cheap: a full node rebuilds an old block's trie from the head, and on Sepolia
        // that took longer than any timeout once the route sat idle for a few hours.
        let lagging = self
            .config
            .max_lag
            .is_some_and(|max| step.head.saturating_sub(trusted.height) >= max);
        if !worth_attesting(&messages, self.destination_domain, &self.config.routers)
            && !heartbeat
            && !lagging
        {
            debug!(route = %self.config.name, leaves = messages.len(), "nothing for our routes");
            self.record_activity(
                "waiting",
                format!(
                    "{} new leaves since height {}, none for our routers",
                    messages.len(),
                    trusted.height
                ),
            );
            return Ok(None);
        }

        if messages.is_empty() {
            info!(route = %self.config.name, from = trusted.height, to = step.head, "heartbeat: advancing with no messages");
        }
        self.record_activity(
            "attesting",
            if messages.is_empty() {
                format!(
                    "heartbeat: asking the enclave to attest heights {} to {} with no messages",
                    trusted.height, step.head
                )
            } else {
                format!(
                    "asking the enclave to attest heights {} to {} with {} messages",
                    trusted.height,
                    step.head,
                    messages.len()
                )
            },
        );
        debug!(route = %self.config.name, from = trusted.height, to = step.head, leaves = messages.len(), "attesting");
        // Each message proves itself at delivery with a branch against the root the enclave is
        // about to attest. Built here, from the tree at the trusted height and the leaves just
        // indexed, and checked against the head's tree before the enclave is asked: an index
        // that disagrees with the chain is caught where it can be named.
        let (frontier, metadata) = proofs(&step, &trusted, &messages, self.destination_domain)?;
        let request = json!({
            "protocol": tee_node::attest::PROTOCOL_VERSION,
            "trusted_state": hex::encode(trusted.encode()),
            "chain": step.chain,
            "input": step.input,
            "tree": step.tree,
            "merkle_tree_address": step.tree_address,
        });
        let (attestation, enclave) = self.enclaves.attest(&request).await?;
        let new_state = tee_node::state::IsmState::decode(&hex::decode(
            attestation.new_state.trim_start_matches("0x"),
        )?)?;
        // The enclave attests an unchanged root whenever asked; the ISMs refuse one that moves
        // the origin clock too little. Nothing to stage then: the next tick finds a later head.
        if too_soon(&trusted, &new_state, !messages.is_empty()) {
            debug!(route = %self.config.name, head = step.head, "too soon for an attestation with no new leaves");
            self.record_activity(
                "waiting",
                format!(
                    "no new leaves and the origin clock moved only {}s since height {}; the ISM wants {}s",
                    new_state.timestamp.saturating_sub(trusted.timestamp),
                    trusted.height,
                    EMPTY_BATCH_MIN_INTERVAL_SECS
                ),
            );
            return Ok(None);
        }
        if let Some(first) = metadata.iter().find(|m| !m.is_empty()) {
            anyhow::ensure!(
                attestation.tree_root.trim_start_matches("0x") == hex::encode(&first[..32]),
                "the enclave attested tree root {} but the proofs were built for {}",
                attestation.tree_root,
                hex::encode(&first[..32])
            );
        }
        let record = json!({
            "attestation": {
                "quote": attestation.quote,
                "event_log": attestation.event_log,
                "payload": attestation.payload,
                "new_state": attestation.new_state,
                "tree_root": attestation.tree_root,
                "tree_count": attestation.tree_count,
            },
            "messages": messages.iter().map(|m| hex::encode(&m.bytes)).collect::<Vec<_>>(),
            "metadata": metadata.iter().map(hex::encode).collect::<Vec<_>>(),
            "from_height": trusted.height,
            "frontier": frontier,
            "enclave": enclave,
        });
        std::fs::write(self.staged(), serde_json::to_vec_pretty(&record)?)?;
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
        let hex_list = |v: &Value| -> Vec<Vec<u8>> {
            v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| hex::decode(m.as_str()?).ok())
                .collect()
        };
        let batch = Batch {
            quote: text(&att["quote"])?,
            event_log: text(&att["event_log"])?,
            payload: hex::decode(text(&att["payload"])?.trim_start_matches("0x"))?,
            messages: hex_list(&record["messages"]),
            metadata: hex_list(&record["metadata"]),
        };
        // A batch staged by an earlier relayer carried message ids, which no ISM accepts any
        // more, and no metadata, so its messages could not be delivered either. Rebuild.
        let update = match tee_node::state::AttestedUpdate::decode(&batch.payload) {
            Ok(u) if batch.metadata.len() == batch.messages.len() => u,
            _ => {
                warn!(route = %self.config.name, "discarding a staged batch in the previous format; rebuilding");
                std::fs::remove_file(&path)?;
                return Ok(None);
            }
        };
        let provenance = serde_json::from_value::<MerkleTree>(record["frontier"].clone())
            .ok()
            .map(|frontier| Provenance {
                from_height: update.prev_state.height,
                frontier,
            });
        if too_soon(
            &update.prev_state,
            &update.new_state,
            !batch.messages.is_empty(),
        ) {
            // Carries no messages (the root did not move), so nothing to keep.
            warn!(route = %self.config.name, "discarding a staged batch the ISM would refuse as too soon; rebuilding later");
            std::fs::remove_file(&path)?;
            return Ok(None);
        }
        self.record_activity(
            "submitting",
            format!(
                "submitting the attestation for heights {} to {} ({} messages) and delivering",
                update.prev_state.height,
                update.new_state.height,
                batch.messages.len()
            ),
        );
        if age > STAGED_BATCH_MAX_AGE {
            warn!(route = %self.config.name, age_secs = age.as_secs(), "discarding a staged batch too old to be accepted; rebuilding");
            self.discard(&batch, provenance.as_ref(), &path).await?;
            return Ok(None);
        }
        let submitted = match self.destination.submit(&batch).await {
            Ok(d) => d,
            // The ISM moved past this batch's start: someone else attested first. No retry can
            // land it, and its root may never be attested, so its messages are kept with what
            // re-proves them. The next pass builds from where the ISM actually is.
            Err(e) if e.is::<Stale>() => {
                warn!(route = %self.config.name, "discarding a batch the ISM has moved past; rebuilding");
                self.discard(&batch, provenance.as_ref(), &path).await?;
                return Ok(None);
            }
            // The ISM no longer pins the enclave that signed this batch, as after a rotation
            // dropped it. No retry can land it; the next pass asks another enclave first.
            Err(e) if refused_enclave(&e) => {
                let enclave = record["enclave"].as_str().unwrap_or_default().to_string();
                warn!(route = %self.config.name, %enclave, "the ISM does not pin the enclave that signed this batch; rebuilding with another");
                self.enclaves.refused(&enclave, &format!("{e:#}"));
                self.discard(&batch, provenance.as_ref(), &path).await?;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        for d in submitted.deliveries {
            self.settle(d, provenance.as_ref());
        }

        let height = update.new_state.height;
        // Every leaf of this batch is now delivered or parked, so the relayer has accounted for
        // the origin up to its height, provided it had up to where the batch starts. If not,
        // the next pass reconciles the whole gap instead.
        if let (Some(p), Some(covered)) = (&provenance, self.covered()) {
            let leaves: Vec<[u8; 32]> = batch
                .messages
                .iter()
                .map(|m| alloy_primitives::keccak256(m).0)
                .collect();
            if covered.height == p.from_height {
                if let Ok(frontier) =
                    replay(&p.frontier, &leaves, update.tree_root, update.tree_count)
                {
                    self.set_covered(height, &frontier);
                }
            }
        }
        // What the dashboard lists for each attestation.
        record["from_height"] = update.prev_state.height.into();
        record["attested_at"] = update.attested_at.into();
        record["submitted_at"] = now().into();
        record["attestation_tx"] = submitted.attestation_tx.clone().into();
        record["heartbeat"] = batch.messages.is_empty().into();
        record["height"] = height.into();
        record["state_root"] = format!("0x{}", hex::encode(update.new_state.state_root)).into();
        record["tree_root"] = format!("0x{}", hex::encode(update.tree_root)).into();
        record["tree_count"] = update.tree_count.into();
        record["quote"] = batch.quote.clone().into();
        record["measurements"] =
            serde_json::to_value(crate::api::measurements(&batch.quote, &batch.event_log)?)?;
        record["batch"] = batch
            .messages
            .iter()
            .map(|m| format!("0x{}", hex::encode(alloy_primitives::keccak256(m))))
            .collect::<Vec<_>>()
            .into();
        std::fs::write(
            self.dir.join(format!("{height}.json")),
            serde_json::to_vec_pretty(&record)?,
        )?;
        std::fs::remove_file(&path)?;
        self.record_activity(
            "advanced",
            format!(
                "the ISM moved from height {} to {}{}",
                update.prev_state.height,
                height,
                match &submitted.attestation_tx {
                    Some(tx) => format!(" in {tx}"),
                    None => String::new(),
                }
            ),
        );
        Ok(Some(height))
    }

    /// Drop a staged batch that can no longer land, keeping what it owes: every message of ours
    /// in it that is not delivered is parked with its proof and where that proof came from.
    /// Redelivery lands it once the ISM covers it, re-proving it first if the ISM holds another
    /// root than the one it was built for; until then the next step indexes it again, and the
    /// parked copy is found delivered.
    async fn discard(
        &self,
        batch: &Batch,
        provenance: Option<&Provenance>,
        path: &std::path::Path,
    ) -> Result<()> {
        for (id, message, metadata) in batch.for_domain(self.destination_domain) {
            if !self.destination.delivered(&id).await? {
                warn!(route = %self.config.name, id, "parking a message from a discarded batch");
                self.park(
                    &id,
                    message,
                    metadata,
                    provenance,
                    "left over from a batch that could not finish".into(),
                    None,
                );
            }
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    /// Record what became of one message.
    fn settle(&self, d: Delivery, provenance: Option<&Provenance>) {
        match d.outcome {
            Outcome::Delivered { tx } => self.mark_delivered(&d.id, Some(tx)),
            Outcome::AlreadyDelivered => self.mark_delivered(&d.id, None),
            Outcome::Refused { tx, reason } => {
                warn!(route = %self.config.name, id = d.id, reason, "delivery refused; parked for redelivery");
                self.park(&d.id, &d.message, &d.metadata, provenance, reason, tx);
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

    /// Keep a message for redelivery. `provenance` is kept from the earlier record when none
    /// is given, so a retry never loses what re-proves the message.
    fn park(
        &self,
        id: &str,
        message: &[u8],
        metadata: &[u8],
        provenance: Option<&Provenance>,
        reason: String,
        tx: Option<String>,
    ) {
        let path = self.dir.join("undelivered").join(format!("{id}.json"));
        let at = now();
        let previous: Option<Parked> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let attempts = previous.as_ref().map_or(0, |p| p.attempts) + 1;
        let body = Parked {
            id: id.to_string(),
            message: hex::encode(message),
            metadata: hex::encode(metadata),
            provenance: provenance
                .cloned()
                .or_else(|| previous.as_ref().and_then(|p| p.provenance.clone())),
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
            let Ok(mut metadata) = hex::decode(&parked.metadata) else {
                continue;
            };
            // A proof lands only against the root it was built for. If the ISM never attested
            // that root (someone else's attestation moved it past ours, or it was replaced and
            // started over), build one against the root it holds now, which covers every
            // earlier leaf.
            match self.current_proof(&parked, &metadata).await {
                Ok(Some(fresh)) => {
                    info!(route = %self.config.name, id = parked.id, "re-proved against the root the ISM holds");
                    metadata = fresh;
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(route = %self.config.name, id = parked.id, error = %format!("{e:#}"), "cannot re-prove yet");
                    self.park(
                        &parked.id,
                        &message,
                        &metadata,
                        None,
                        format!("{e:#}"),
                        parked.tx,
                    );
                    continue;
                }
            }
            match self.destination.deliver(&message, &metadata).await {
                Ok(outcome) => {
                    if matches!(
                        outcome,
                        Outcome::Delivered { .. } | Outcome::AlreadyDelivered
                    ) {
                        info!(route = %self.config.name, id = parked.id, attempts = parked.attempts + 1, "parked message delivered");
                    }
                    self.settle(
                        Delivery {
                            id: parked.id,
                            message,
                            metadata,
                            outcome,
                        },
                        None,
                    )
                }
                Err(e) => {
                    warn!(route = %self.config.name, id = parked.id, error = %format!("{e:#}"), "redelivery failed");
                    self.park(
                        &parked.id,
                        &message,
                        &metadata,
                        None,
                        format!("{e:#}"),
                        parked.tx,
                    );
                }
            }
        }
    }

    /// A proof for a parked message against the root the ISM holds now, when the one it has
    /// is against a root the ISM never attested. `None` when the current proof is fine.
    async fn current_proof(&self, parked: &Parked, metadata: &[u8]) -> Result<Option<Vec<u8>>> {
        anyhow::ensure!(
            metadata.len() == METADATA_BYTES,
            "no proof was recorded for this message (written before roots were attested)"
        );
        let root: [u8; 32] = metadata[..32].try_into()?;
        if self.destination.root_attested(root).await? {
            return Ok(None);
        }
        let index = u32::from_be_bytes(metadata[32..36].try_into()?);
        let provenance = parked
            .provenance
            .as_ref()
            .context("the ISM never attested this proof's root and no record of the tree it was built from was kept")?;
        let (want_root, want_count) = self.destination.tree().await?;
        anyhow::ensure!(
            want_root != [0u8; 32],
            "the ISM has not attested any root yet"
        );
        anyhow::ensure!(
            index < want_count,
            "leaf {index} is past the ISM's tree of {want_count} leaves; the next attestation covers it"
        );
        let ism = self.destination.state().await?;
        let messages = self
            .origin
            .index(provenance.from_height, ism.height)
            .await
            .with_context(|| {
                format!(
                    "indexing heights {} to {} to re-prove leaf {index}",
                    provenance.from_height, ism.height
                )
            })?;
        let ids: Vec<[u8; 32]> = messages.iter().map(|m| m.id).collect();
        reprove(&provenance.frontier, &ids, index, want_root, want_count).map(Some)
    }

    fn covered(&self) -> Option<Covered> {
        std::fs::read(self.dir.join("covered.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
    }

    fn set_covered(&self, height: u64, frontier: &MerkleTree) {
        let body = Covered {
            height,
            frontier: *frontier,
        };
        if let Err(e) = write_json(&self.dir.join("covered.json"), &body) {
            warn!(route = %self.config.name, error = %e, "could not record how far the route is covered");
        }
    }

    /// Account for every leaf between the height this relayer has covered and the ISM's. There
    /// are none when the relayer's own batch moved the ISM. There are some when someone else's
    /// attestation did, before this relayer indexed them: those messages are covered by a root
    /// the ISM holds, and nothing else would ever deliver them. Each one for this destination
    /// that the mailbox has not processed is delivered with a proof against that root.
    async fn reconcile(&self, trusted: &tee_node::state::IsmState) -> Result<()> {
        let Some(covered) = self.covered() else {
            return Ok(());
        };
        if covered.height == trusted.height {
            return Ok(());
        }
        if covered.height > trusted.height {
            // The ISM was replaced by one that starts lower. Start over from it.
            warn!(route = %self.config.name, covered = covered.height, ism = trusted.height, "the ISM is below the height this route had covered; starting over from the ISM");
            let _ = std::fs::remove_file(self.dir.join("covered.json"));
            return Ok(());
        }
        let (root, count) = self.destination.tree().await?;
        anyhow::ensure!(root != [0u8; 32], "the ISM has not attested any root yet");
        let messages = if count == covered.frontier.count {
            Vec::new()
        } else {
            self.origin
                .index(covered.height, trusted.height)
                .await
                .with_context(|| {
                    format!(
                        "indexing heights {} to {} to find messages the ISM was moved past",
                        covered.height, trusted.height
                    )
                })?
        };
        let (frontier, owed) = uncovered(
            &covered.frontier,
            &messages,
            root,
            count,
            self.destination_domain,
        )?;
        let provenance = Provenance {
            from_height: covered.height,
            frontier: covered.frontier,
        };
        for (message, metadata) in owed {
            let id = hex::encode(alloy_primitives::keccak256(&message));
            let file = format!("{id}.json");
            if self.dir.join("delivered").join(&file).exists()
                || self.dir.join("undelivered").join(&file).exists()
            {
                continue;
            }
            if self.destination.delivered(&id).await? {
                self.mark_delivered(&id, None);
                continue;
            }
            // The root was read from the ISM a moment ago, so the proof lands now. Delivered at
            // once rather than parked: a parked message reads as a failed delivery, and nothing
            // has failed. Only a delivery that is refused or cannot be sent is parked.
            warn!(route = %self.config.name, id, from = covered.height, to = trusted.height, "the ISM was moved past a message this relayer had not indexed; delivering it");
            match self.destination.deliver(&message, &metadata).await {
                Ok(outcome) => self.settle(
                    Delivery {
                        id,
                        message,
                        metadata,
                        outcome,
                    },
                    Some(&provenance),
                ),
                Err(e) => self.park(
                    &id,
                    &message,
                    &metadata,
                    Some(&provenance),
                    format!("{e:#}"),
                    None,
                ),
            }
        }
        self.set_covered(trusted.height, &frontier);
        Ok(())
    }

    /// What the route is doing right now, for the dashboard. `since` moves only when the phase
    /// changes, so it says how long the route has been waiting, attesting or submitting.
    fn record_activity(&self, phase: &str, detail: String) {
        let path = self.dir.join("activity.json");
        let now = now();
        let since = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .filter(|old| old["phase"] == phase)
            .and_then(|old| old["since"].as_u64())
            .unwrap_or(now);
        let _ = std::fs::write(
            path,
            json!({ "phase": phase, "detail": detail, "since": since, "at": now }).to_string(),
        );
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

    /// Why the messages below the ISM's height could not be accounted for, in `uncovered.json`.
    /// `since` is when this run of failures began, so the monitor can tell a blip from a gap
    /// that will not read. Logged when it starts; after that the monitor carries it.
    fn record_uncovered(&self, error: &str, ism_height: u64) {
        let path = self.dir.join("uncovered.json");
        let at = now();
        let since = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|b| b["since"].as_u64());
        if since.is_none() {
            warn!(route = %self.config.name, error, "cannot account for the messages below the ISM's height; retrying every pass");
        } else {
            debug!(route = %self.config.name, error, "still cannot account for the messages below the ISM's height");
        }
        let body = json!({
            "error": error,
            "from": self.covered().map(|c| c.height),
            "to": ism_height,
            "since": since.unwrap_or(at),
            "at": at,
        });
        let _ = std::fs::write(path, body.to_string());
    }

    fn clear_uncovered(&self) {
        let path = self.dir.join("uncovered.json");
        if path.exists() {
            info!(route = %self.config.name, "accounted for every message below the ISM's height again");
            let _ = std::fs::remove_file(path);
        }
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
        debug!(route = %route.name, from = %route.from, to = %route.to, "configured");
        tasks.spawn(Route::new(&config, route)?.run(tick));
    }
    let tracker = crate::tracker::Tracker::new(&config)?;
    tracker.spawn(&mut tasks);
    let api = crate::api::Api::new(&config)?;
    let listen = config.api_listen.clone();
    info!(routes = config.routes.len(), api = %listen, "relayer started; a status line follows every 5 minutes");
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

/// The delivery metadata for the messages of a step, `root(32) index(4) proof(32 * 32)` with
/// the head tree's root, which the enclave is about to attest. One entry per message, in order;
/// empty for a message bound elsewhere, which this route never delivers. An origin's tree is
/// shared by every bridge on it, so a step can carry thousands of leaves that are not ours.
///
/// The frontier at the trusted height and the tree at the head are both read through the
/// enclave's own `merkle_tree`, under the roots the ISM holds and moves to, so the proofs are
/// built from state the chain commits to. Replaying the indexed leaves onto the frontier has to
/// reproduce the head's tree: if it does not, the index missed or invented a dispatch, and the
/// step is refused here rather than at delivery.
fn proofs(
    step: &crate::origin::Step,
    trusted: &tee_node::state::IsmState,
    messages: &[Message],
    destination: u32,
) -> Result<(MerkleTree, Vec<Vec<u8>>)> {
    use hyperlane_types::{get_tree_root, insert_leaf};
    let chain = tee_node::origin::find(step.chain)
        .with_context(|| format!("the enclave registry has no chain `{}`", step.chain))?;
    let frontier = trusted_frontier(step, trusted)?;
    let head = chain
        .origin
        .merkle_tree(step.tree.clone(), step.state_root)?
        .tree;
    let ids: Vec<[u8; 32]> = messages.iter().map(|m| m.id).collect();
    let mut replayed = frontier;
    for id in &ids {
        insert_leaf(&mut replayed, *id).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    let root = get_tree_root(&head);
    anyhow::ensure!(
        replayed.count == head.count && get_tree_root(&replayed) == root,
        "the {} indexed leaves on the tree at height {} do not reproduce the tree at height {} \
         ({} leaves); the index missed or invented a dispatch",
        ids.len(),
        trusted.height,
        step.head,
        head.count
    );
    let mut builder = crate::proofs::ProofBuilder::new(&frontier, &ids)?;
    let metadata = messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let ours = hyperlane_types::decode_hyperlane_message(&m.bytes)
                .is_ok_and(|d| d.destination == destination);
            if !ours {
                return Ok(Vec::new());
            }
            let index = frontier.count + i as u32;
            Ok(metadata(root, index, &builder.proof(index)?))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((frontier, metadata))
}

/// The origin tree as of the ISM's trusted height, read through the enclave's own `merkle_tree`
/// under the state root the ISM holds.
fn trusted_frontier(
    step: &crate::origin::Step,
    trusted: &tee_node::state::IsmState,
) -> Result<MerkleTree> {
    let chain = tee_node::origin::find(step.chain)
        .with_context(|| format!("the enclave registry has no chain `{}`", step.chain))?;
    Ok(chain
        .origin
        .merkle_tree(step.tree_snapshot.clone(), trusted.state_root.into())?
        .tree)
}

/// `frontier` with `leaves` inserted, which must be exactly the tree the ISM holds (`root`,
/// `count` leaves): a leaf missed or invented would give proofs against a root nobody attested.
fn replay(
    frontier: &MerkleTree,
    leaves: &[[u8; 32]],
    root: [u8; 32],
    count: u32,
) -> Result<MerkleTree> {
    use hyperlane_types::{get_tree_root, insert_leaf};
    let mut replayed = *frontier;
    for leaf in leaves {
        insert_leaf(&mut replayed, *leaf).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    anyhow::ensure!(
        replayed.count == count && get_tree_root(&replayed) == root,
        "the {} leaves indexed after the {}-leaf frontier give a {}-leaf tree with root {}, but the ISM holds \
         {} leaves under {}",
        leaves.len(),
        frontier.count,
        replayed.count,
        hex::encode(get_tree_root(&replayed)),
        count,
        hex::encode(root)
    );
    Ok(replayed)
}

/// The messages for `destination` among `messages`, the leaves between a covered frontier and
/// the tree the ISM holds, each with a proof against that tree's root; and the tree itself, the
/// next frontier. Every message for the destination, exactly as a batch delivers: the
/// configured routers decide when to attest, never what is delivered.
fn uncovered(
    frontier: &MerkleTree,
    messages: &[Message],
    root: [u8; 32],
    count: u32,
    destination: u32,
) -> Result<(MerkleTree, Vec<(Vec<u8>, Vec<u8>)>)> {
    let ids: Vec<[u8; 32]> = messages.iter().map(|m| m.id).collect();
    let tree = replay(frontier, &ids, root, count)?;
    let mut builder = crate::proofs::ProofBuilder::new(frontier, &ids)?;
    let mut owed = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        let ours = hyperlane_types::decode_hyperlane_message(&m.bytes)
            .is_ok_and(|d| d.destination == destination);
        if !ours {
            continue;
        }
        let index = frontier.count + i as u32;
        owed.push((
            m.bytes.clone(),
            metadata(root, index, &builder.proof(index)?),
        ));
    }
    Ok((tree, owed))
}

/// Prove leaf `index` again, against the tree the ISM holds (`root`, `count` leaves), from the
/// frontier the original proof came from plus the leaves indexed since. The replay must land
/// exactly on the ISM's tree: a leaf missed or invented would be a proof against a root nobody
/// attested.
fn reprove(
    frontier: &MerkleTree,
    leaves: &[[u8; 32]],
    index: u32,
    root: [u8; 32],
    count: u32,
) -> Result<Vec<u8>> {
    replay(frontier, leaves, root, count)?;
    let mut builder = crate::proofs::ProofBuilder::new(frontier, leaves)?;
    Ok(metadata(root, index, &builder.proof(index)?))
}

/// Would both ISMs refuse this transition? They require an attestation that leaves the tree
/// root where it is to move the origin clock by `EMPTY_BATCH_MIN_INTERVAL_SECS`.
fn too_soon(
    prev: &tee_node::state::IsmState,
    next: &tee_node::state::IsmState,
    root_changed: bool,
) -> bool {
    !root_changed && next.timestamp < prev.timestamp + EMPTY_BATCH_MIN_INTERVAL_SECS
}

/// What a delivery hands the ISM: `root(32) index(4 BE) proof(32 * 32)`, 1060 bytes, as both
/// `TeeDcapIsm.verify` and `x/teeism`'s `Verify` read it.
const METADATA_BYTES: usize = 32 + 4 + 32 * hyperlane_types::TREE_DEPTH;

fn metadata(
    root: [u8; 32],
    index: u32,
    proof: &[[u8; 32]; hyperlane_types::TREE_DEPTH],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + 4 + 32 * proof.len());
    out.extend_from_slice(&root);
    out.extend_from_slice(&index.to_be_bytes());
    for node in proof {
        out.extend_from_slice(node);
    }
    out
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

/// Whether the ISM's trusted state is old enough for a heartbeat. Read from the state itself,
/// the origin's own clock at the last advance, so it holds across restarts and hosts.
fn heartbeat_due(trusted_timestamp: u64, now: u64) -> bool {
    now.saturating_sub(trusted_timestamp) >= HEARTBEAT.as_secs()
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
    /// The origin tree's root and leaf count at that state, hex and plain.
    pub tree_root: String,
    pub tree_count: u32,
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

/// How the ISMs say a quote is not from an enclave they pin: `TeeDcapIsm`'s
/// `WrongEnclave(bytes32)` (by name, or its selector in raw revert data) and `x/teeism`'s
/// ErrIdentityMismatch.
const ENCLAVE_REFUSALS: &[&str] = &[
    "WrongEnclave",
    "466bb58c",
    "quote does not come from a pinned enclave",
];

fn refused_enclave(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    ENCLAVE_REFUSALS.iter().any(|r| text.contains(r))
}

/// One enclave's recent record, as the dashboard and the monitor read it from `enclaves.json`.
#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnclaveHealth {
    pub url: String,
    pub last_ok_at: Option<u64>,
    pub last_error_at: Option<u64>,
    pub last_error: Option<String>,
}

impl EnclaveHealth {
    /// Healthy unless its last word was an error.
    pub fn healthy(&self) -> bool {
        match (self.last_ok_at, self.last_error_at) {
            (_, None) => true,
            (Some(ok), Some(err)) => ok >= err,
            (None, Some(_)) => false,
        }
    }
}

/// The enclaves that may attest a route. Any one of them is enough, because the ISM pins them
/// all; they are asked in turn, starting with the last that answered, so one that is down or
/// being rotated out costs a retry, not the route.
struct EnclavePool {
    clients: Vec<EnclaveClient>,
    preferred: std::sync::atomic::AtomicUsize,
    health: std::sync::Mutex<Vec<EnclaveHealth>>,
    file: PathBuf,
}

impl EnclavePool {
    fn new(urls: &[String], file: PathBuf) -> Self {
        let clients: Vec<EnclaveClient> = urls.iter().map(EnclaveClient::new).collect();
        let health = clients
            .iter()
            .map(|c| EnclaveHealth {
                url: c.url.clone(),
                ..Default::default()
            })
            .collect();
        let pool = Self {
            clients,
            preferred: Default::default(),
            health: std::sync::Mutex::new(health),
            file,
        };
        pool.write();
        pool
    }

    /// Attest with the first enclave that answers, returning its URL with the attestation.
    async fn attest(&self, request: &serde_json::Value) -> Result<(Attestation, String)> {
        use std::sync::atomic::Ordering;
        let n = self.clients.len();
        let start = self.preferred.load(Ordering::Relaxed) % n;
        let mut failures = Vec::new();
        for k in 0..n {
            let i = (start + k) % n;
            let client = &self.clients[i];
            match client.attest(request).await {
                Ok(a) => {
                    if k > 0 {
                        warn!(enclave = %client.url, "attested by a fallback enclave");
                    }
                    self.preferred.store(i, Ordering::Relaxed);
                    self.note(i, None);
                    return Ok((a, client.url.clone()));
                }
                Err(e) => {
                    let e = format!("{e:#}");
                    if n > 1 {
                        warn!(enclave = %client.url, error = %e, "enclave failed; trying the next");
                    }
                    self.note(i, Some(e.clone()));
                    failures.push(format!("{}: {e}", client.url));
                }
            }
        }
        anyhow::bail!("no enclave attested: {}", failures.join("; "))
    }

    /// The ISM refused what `url` signed: mark it and move on to the next enclave.
    fn refused(&self, url: &str, error: &str) {
        use std::sync::atomic::Ordering;
        if let Some(i) = self.clients.iter().position(|c| c.url == url) {
            self.note(
                i,
                Some(format!("the ISM does not pin this enclave: {error}")),
            );
            self.preferred
                .store((i + 1) % self.clients.len(), Ordering::Relaxed);
        }
    }

    fn note(&self, i: usize, error: Option<String>) {
        let at = now();
        if let Ok(mut h) = self.health.lock() {
            match error {
                None => h[i].last_ok_at = Some(at),
                Some(e) => {
                    h[i].last_error_at = Some(at);
                    h[i].last_error = Some(e);
                }
            }
        }
        self.write();
    }

    fn write(&self) {
        if let Ok(h) = self.health.lock() {
            if let Ok(json) = serde_json::to_vec_pretty(&*h) {
                let _ = std::fs::write(&self.file, json);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperlane_types::{encode_hyperlane_message, HyperlaneMessage};

    #[test]
    fn both_isms_refusals_of_an_unpinned_enclave_are_recognised() {
        for text in [
            "attestation reverted: WrongEnclave(0x12)",
            "execution reverted, data: \"0x466bb58c0000\"",
            "tx failed: quote does not come from a pinned enclave: none of 2 pinned identities match",
        ] {
            assert!(refused_enclave(&anyhow::anyhow!(text.to_string())), "{text}");
        }
        assert!(!refused_enclave(&anyhow::anyhow!("TrustedStateMismatch")));
    }

    #[test]
    fn an_enclave_is_healthy_unless_its_last_word_was_an_error() {
        let h = |ok, err| EnclaveHealth {
            url: String::new(),
            last_ok_at: ok,
            last_error_at: err,
            last_error: None,
        };
        assert!(h(None, None).healthy());
        assert!(h(Some(5), Some(4)).healthy());
        assert!(!h(Some(4), Some(5)).healthy());
        assert!(!h(None, Some(1)).healthy());
    }

    /// Every enclave is tried before the route gives up, and each failure is recorded.
    #[tokio::test]
    async fn the_pool_asks_every_enclave_before_failing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("enclaves.json");
        // Nothing listens on port 9: both fail fast.
        let urls = vec![
            "http://127.0.0.1:9/a".to_string(),
            "http://127.0.0.1:9/b".to_string(),
        ];
        let pool = EnclavePool::new(&urls, file.clone());
        let err = pool
            .attest(&serde_json::json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("/a") && err.contains("/b"), "{err}");
        let health: Vec<EnclaveHealth> =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(health.len(), 2);
        assert!(health.iter().all(|h| !h.healthy()));

        pool.refused("http://127.0.0.1:9/a", "WrongEnclave");
        assert_eq!(pool.preferred.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

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
    fn the_heartbeat_is_due_once_the_trusted_state_is_a_day_old() {
        let day = HEARTBEAT.as_secs();
        assert!(!heartbeat_due(1_000, 1_000), "just advanced");
        assert!(!heartbeat_due(1_000, 1_000 + day - 1), "not yet a day");
        assert!(heartbeat_due(1_000, 1_000 + day), "a day old");
        assert!(
            day >= 2 * EMPTY_BATCH_MIN_INTERVAL_SECS,
            "the heartbeat must clear the ISMs' minimum interval with room for origin lag"
        );
    }

    /// The layout both ISMs parse, byte for byte.
    #[test]
    fn delivery_metadata_is_root_index_proof() {
        let mut proof = [[0u8; 32]; hyperlane_types::TREE_DEPTH];
        proof[0] = [0xaa; 32];
        proof[31] = [0xbb; 32];
        let m = metadata([0x11; 32], 0x0102_0304, &proof);
        assert_eq!(m.len(), 1060);
        assert_eq!(&m[..32], &[0x11; 32]);
        assert_eq!(&m[32..36], &[1, 2, 3, 4]);
        assert_eq!(&m[36..68], &[0xaa; 32]);
        assert_eq!(&m[1028..], &[0xbb; 32]);
    }

    fn proof_in(metadata: &[u8]) -> [[u8; 32]; hyperlane_types::TREE_DEPTH] {
        let mut p = [[0u8; 32]; hyperlane_types::TREE_DEPTH];
        for (node, chunk) in p.iter_mut().zip(metadata[36..].chunks(32)) {
            node.copy_from_slice(chunk);
        }
        p
    }

    /// Someone else attested past us: our proof names a root the ISM never recorded. Built
    /// again from the frontier we proved from plus the leaves indexed since, the same message
    /// proves against the root the ISM holds.
    #[test]
    fn a_message_is_re_proven_against_the_root_the_ism_holds() {
        use hyperlane_types::{get_branch_root, get_tree_root, insert_leaf, keccak256};
        let leaves: Vec<[u8; 32]> = (0u32..12).map(|i| keccak256(&i.to_be_bytes())).collect();
        let tree = |n: usize| {
            let mut t = MerkleTree::default();
            for l in &leaves[..n] {
                insert_leaf(&mut t, *l).unwrap();
            }
            t
        };
        let frontier = tree(3); // the ISM's trusted height when we built our step
        let ours = tree(7); // our head: leaves 3..7
        let theirs = tree(12); // what the ISM attested instead
        let index = 5u32;
        let stale = {
            let mut b = crate::proofs::ProofBuilder::new(&frontier, &leaves[3..7]).unwrap();
            metadata(get_tree_root(&ours), index, &b.proof(index).unwrap())
        };
        let their_root = get_tree_root(&theirs);
        assert_ne!(
            get_branch_root(leaves[5], &proof_in(&stale), index),
            their_root,
            "the stale proof cannot land under the ISM's root"
        );

        let fresh = reprove(&frontier, &leaves[3..12], index, their_root, 12).unwrap();
        assert_eq!(fresh.len(), METADATA_BYTES);
        assert_eq!(&fresh[..32], &their_root);
        assert_eq!(&fresh[32..36], &index.to_be_bytes());
        assert_eq!(
            get_branch_root(leaves[5], &proof_in(&fresh), index),
            their_root
        );

        assert!(
            reprove(&frontier, &leaves[3..12], 12, their_root, 12).is_err(),
            "a leaf the ISM does not cover yet"
        );
        assert!(
            reprove(&frontier, &leaves[3..11], index, their_root, 12)
                .unwrap_err()
                .to_string()
                .contains("but the ISM holds"),
            "a replay that misses a leaf"
        );
        assert!(
            reprove(&frontier, &leaves[3..12], 2, their_root, 12).is_err(),
            "a leaf before the frontier"
        );
    }

    /// Someone else attested past messages this relayer never indexed. The leaves between what
    /// it had covered and the ISM's tree are indexed, and each of ours comes back with a proof
    /// that lands under the root the ISM holds.
    #[test]
    fn messages_the_ism_was_moved_past_are_found_and_proven() {
        use hyperlane_types::{get_branch_root, get_tree_root, insert_leaf, keccak256};
        let ours = [7u8; 32];
        let with_id = |destination, recipient, nonce: u32| {
            let mut m = message(destination, recipient);
            m.bytes.extend_from_slice(&nonce.to_be_bytes());
            m.id = keccak256(&m.bytes);
            m
        };
        let before: Vec<[u8; 32]> = (0u32..3).map(|i| keccak256(&i.to_be_bytes())).collect();
        let mut frontier = MerkleTree::default();
        for l in &before {
            insert_leaf(&mut frontier, *l).unwrap();
        }
        // Leaves 3..7: for another chain, for this one, for another chain, for this one.
        let gap = vec![
            with_id(6, [1; 32], 0),
            with_id(5, ours, 1),
            with_id(6, ours, 2),
            with_id(5, [1; 32], 3),
        ];
        let mut ism = frontier;
        for m in &gap {
            insert_leaf(&mut ism, m.id).unwrap();
        }
        let root = get_tree_root(&ism);

        let (tree, owed) = uncovered(&frontier, &gap, root, 7, 5).unwrap();
        assert_eq!(
            get_tree_root(&tree),
            root,
            "the next frontier is the ISM's tree"
        );
        assert_eq!(tree.count, 7);
        assert_eq!(
            owed.len(),
            2,
            "the two for this destination, whoever receives them"
        );
        for ((bytes, metadata), (index, m)) in owed.iter().zip([(4u32, &gap[1]), (6, &gap[3])]) {
            assert_eq!(bytes, &m.bytes);
            assert_eq!(metadata.len(), METADATA_BYTES);
            assert_eq!(&metadata[..32], &root);
            assert_eq!(&metadata[32..36], &index.to_be_bytes());
            assert_eq!(get_branch_root(m.id, &proof_in(metadata), index), root);
        }

        // Nothing dispatched in the gap: the frontier is already the ISM's tree.
        let (same, none) = uncovered(&frontier, &[], get_tree_root(&frontier), 3, 5).unwrap();
        assert_eq!(same.count, 3);
        assert!(none.is_empty());
        // An index that misses a leaf is refused rather than proven against the wrong tree.
        assert!(uncovered(&frontier, &gap[..3], root, 7, 5)
            .unwrap_err()
            .to_string()
            .contains("but the ISM holds"));
    }

    #[test]
    fn an_attestation_with_no_new_leaves_waits_for_the_isms_interval() {
        let s = |t: u64| tee_node::state::IsmState {
            state_root: [0; 32],
            origin_domain: 1,
            height: 1,
            timestamp: t,
            lc_store_commit: [0; 32],
            identity_digest: [0; 32],
        };
        let gap = EMPTY_BATCH_MIN_INTERVAL_SECS;
        assert!(too_soon(&s(1000), &s(1000 + gap - 1), false));
        assert!(!too_soon(&s(1000), &s(1000 + gap), false));
        assert!(
            !too_soon(&s(1000), &s(1001), true),
            "new leaves move the root; no interval applies"
        );
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
