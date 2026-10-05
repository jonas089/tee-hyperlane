//! Decides when transfers and routes are stuck, and keeps the inbox of notifications.

use serde::{Deserialize, Serialize};

use crate::route::{now, Parked};
use crate::tracker::{route_head, MessageRecord, TrackedRoute, Tracker};

/// Verified but not delivered for this long is stuck. Delivery follows verification in the
/// same pass, so this is many retries.
pub const DELIVERY_GRACE: u64 = 10 * 60;
/// Attestable but not verified for this long is stuck. One attestation takes minutes.
pub const ATTEST_GRACE: u64 = 20 * 60;
/// A route loop, origin watch or ISM read failing for this long is reported.
pub const FAILING_GRACE: u64 = 10 * 60;
/// A route loop that has written nothing for this long has stopped. Its longest pause between
/// passes is 30 minutes of backoff.
pub const LOOP_SILENCE: u64 = 45 * 60;
/// A staged batch older than this has been trying to land for too long.
pub const STAGED_GRACE: u64 = 30 * 60;
/// Resolved notifications are kept this long.
const KEEP_RESOLVED: u64 = 7 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Dispatched, not yet covered by the ISM, and within its route's normal time.
    Pending,
    /// Covered by the ISM; delivery is next.
    Verified,
    Delivered,
    /// Delivery was refused; it is parked and retried.
    Failed,
    /// Past the point where it should have moved on.
    Overdue,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Assessment {
    pub status: Status,
    /// When it should be delivered by, at the latest.
    pub expected_by: u64,
    /// What it is waiting on, while pending or verified.
    pub waiting_on: Option<String>,
    /// What is wrong, when failed or overdue.
    pub problem: Option<String>,
}

pub fn assess(
    record: &MessageRecord,
    route: &TrackedRoute,
    origin_kind: &str,
    parked: Option<&Parked>,
    at: u64,
) -> Assessment {
    let dispatched = record
        .dispatch
        .as_ref()
        .map_or(record.first_seen_at, |d| d.timestamp);
    let expected_by = dispatched + route.expected_latency;
    let verdict = |status, waiting: Option<String>, problem: Option<String>| Assessment {
        status,
        expected_by,
        waiting_on: waiting,
        problem,
    };
    if record.delivery.is_some() {
        return verdict(Status::Delivered, None, None);
    }
    if let Some(p) = parked {
        return verdict(
            Status::Failed,
            None,
            Some(format!(
                "delivery refused {} time{} over {}: {}; next retry in {}",
                p.attempts,
                if p.attempts == 1 { "" } else { "s" },
                duration(at.saturating_sub(p.first_failed_at)),
                p.reason,
                duration(p.next_attempt_at.saturating_sub(at))
            )),
        );
    }
    if let Some(v) = &record.verified {
        let waited = at.saturating_sub(v.at);
        if waited > DELIVERY_GRACE {
            return verdict(
                Status::Overdue,
                None,
                Some(format!(
                    "verified by the ISM at height {} but not delivered after {}",
                    v.ism_height,
                    duration(waited)
                )),
            );
        }
        return verdict(Status::Verified, Some("delivery".into()), None);
    }
    if let Some(a) = record.attestable_at {
        let waited = at.saturating_sub(a);
        if waited > ATTEST_GRACE {
            return verdict(
                Status::Overdue,
                None,
                Some(format!(
                    "the origin has finalized its block, but the ISM has not advanced to it after {}",
                    duration(waited)
                )),
            );
        }
    }
    if at > expected_by {
        return verdict(
            Status::Overdue,
            None,
            Some(format!(
                "not verified {} after dispatch; this route should take at most {}",
                duration(at - dispatched),
                duration(route.expected_latency)
            )),
        );
    }
    verdict(
        Status::Pending,
        Some(crate::tracker::waiting_on(origin_kind).into()),
        None,
    )
}

pub fn duration(secs: u64) -> String {
    match secs {
        s if s < 90 => format!("{s}s"),
        s if s < 90 * 60 => format!("{}m", s / 60),
        s if s < 48 * 3600 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
    }
}

// ---------------------------------------------------------------- the inbox

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Subject {
    Message,
    Route,
    Chain,
    Monitor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    /// Unique per occurrence: the condition's key and when it opened.
    pub id: String,
    /// The condition, such as `message-overdue:0x…`. One open notification per key.
    pub key: String,
    pub severity: Severity,
    pub subject: Subject,
    /// The message id, route name or chain name it is about.
    pub subject_id: String,
    pub title: String,
    pub detail: String,
    pub opened_at: u64,
    pub updated_at: u64,
    pub resolved_at: Option<u64>,
}

/// A condition that holds right now.
struct Condition {
    key: String,
    severity: Severity,
    subject: Subject,
    subject_id: String,
    title: String,
    detail: String,
}

/// Every problem there is right now.
fn conditions(tracker: &Tracker, at: u64) -> Vec<Condition> {
    let mut out = Vec::new();
    let state = tracker.lock();

    for route in &tracker.routes {
        let origin_kind = tracker.chain(&route.from).map_or("", |c| c.kind.as_str());
        let parked = tracker.parked_all(route);

        // A parked message is reported whether or not the tracker has a record of it.
        for p in &parked {
            let id = format!("0x{}", p.id.trim_start_matches("0x"));
            out.push(Condition {
                key: format!("message-failed:{id}"),
                severity: Severity::Critical,
                subject: Subject::Message,
                subject_id: id,
                title: format!("Delivery failing on {}", route.name),
                detail: format!(
                    "refused {} time{}: {}",
                    p.attempts,
                    if p.attempts == 1 { "" } else { "s" },
                    p.reason
                ),
            });
        }
        for record in state
            .messages
            .values()
            .filter(|m| m.route == route.name && m.delivery.is_none())
        {
            let is_parked = parked
                .iter()
                .any(|p| p.id.trim_start_matches("0x") == record.id.trim_start_matches("0x"));
            if is_parked {
                continue;
            }
            let a = assess(record, route, origin_kind, None, at);
            if a.status == Status::Overdue {
                out.push(Condition {
                    key: format!("message-overdue:{}", record.id),
                    severity: Severity::Critical,
                    subject: Subject::Message,
                    subject_id: record.id.clone(),
                    title: format!("Transfer stuck on {}", route.name),
                    detail: a.problem.unwrap_or_default(),
                });
            }
        }

        // The route loop: failing, or silent.
        let blocked: Option<serde_json::Value> =
            crate::tracker::read_json(&route.dir.join("blocked.json"));
        let failing_since = blocked
            .as_ref()
            .and_then(|b| b["since"].as_u64().or(b["at"].as_u64()));
        if let (Some(b), Some(since)) = (&blocked, failing_since) {
            let for_secs = at.saturating_sub(since);
            if for_secs > FAILING_GRACE {
                out.push(Condition {
                    key: format!("route-failing:{}", route.name),
                    severity: if for_secs > 6 * FAILING_GRACE {
                        Severity::Critical
                    } else {
                        Severity::Warning
                    },
                    subject: Subject::Route,
                    subject_id: route.name.clone(),
                    title: format!("{} failing for {}", route.name, duration(for_secs)),
                    detail: b["error"].as_str().unwrap_or("").to_string(),
                });
            }
        }
        let last_activity = [
            route_head(&route.dir).map(|h| h.1),
            blocked.as_ref().and_then(|b| b["at"].as_u64()),
        ]
        .into_iter()
        .flatten()
        .max();
        match last_activity {
            Some(t) if at.saturating_sub(t) > LOOP_SILENCE => out.push(Condition {
                key: format!("route-silent:{}", route.name),
                severity: Severity::Critical,
                subject: Subject::Route,
                subject_id: route.name.clone(),
                title: format!("{} has stopped running", route.name),
                detail: format!(
                    "the route loop has reported nothing for {}; the relayer may be hung",
                    duration(at - t)
                ),
            }),
            _ => {}
        }

        // A batch that has been trying to land for too long.
        if let Ok(meta) = std::fs::metadata(route.dir.join("staging").join("batch.json")) {
            let age = meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map_or(0, |d| d.as_secs());
            if age > STAGED_GRACE {
                out.push(Condition {
                    key: format!("route-staged:{}", route.name),
                    severity: Severity::Warning,
                    subject: Subject::Route,
                    subject_id: route.name.clone(),
                    title: format!("{} has a batch waiting to land", route.name),
                    detail: format!(
                        "an attested batch has been submitting for {}",
                        duration(age)
                    ),
                });
            }
        }

        // The ISM itself cannot be read.
        if let Some(ism) = state.isms.get(&route.name) {
            if let Some(since) = ism.failing_since {
                if at.saturating_sub(since) > FAILING_GRACE {
                    out.push(Condition {
                        key: format!("ism-unreadable:{}", route.name),
                        severity: Severity::Warning,
                        subject: Subject::Route,
                        subject_id: route.name.clone(),
                        title: format!("Cannot read {}'s ISM", route.name),
                        detail: ism.error.clone().unwrap_or_default(),
                    });
                }
            }
        }
    }

    // A gas wallet running dry stops its destination's routes, so it is reported before it does.
    for w in &tracker.wallets {
        let reading = state.wallets.get(&w.chain).cloned().unwrap_or_default();
        let (level, _, problem) = crate::wallets::judge(w, &reading);
        let severity = match level {
            crate::wallets::Level::Critical => Some(Severity::Critical),
            crate::wallets::Level::Low => Some(Severity::Warning),
            _ => None,
        };
        if let (Some(severity), Some(detail)) = (severity, problem) {
            out.push(Condition {
                key: format!("wallet-low:{}", w.chain),
                severity,
                subject: Subject::Chain,
                subject_id: w.chain.clone(),
                title: format!("Top up the relayer on {}", w.chain),
                detail: format!(
                    "{detail}; address {}",
                    reading.address.as_deref().unwrap_or("unknown")
                ),
            });
        }
        if reading
            .failing_since
            .is_some_and(|s| at.saturating_sub(s) > FAILING_GRACE)
        {
            out.push(Condition {
                key: format!("wallet-unreadable:{}", w.chain),
                severity: Severity::Warning,
                subject: Subject::Chain,
                subject_id: w.chain.clone(),
                title: format!("Cannot read the relayer's wallet on {}", w.chain),
                detail: reading.error.clone().unwrap_or_default(),
            });
        }
    }

    // An origin the tracker cannot watch is a blind spot: a transfer from it could be stuck
    // with nothing else saying so.
    // Every origin counts, including one whose watcher has never finished a read, which has no
    // entry at all: from startup, it gets the same grace as a failure.
    for chain in tracker.origins() {
        let none = crate::tracker::Watch::default();
        let watch = state.watches.get(chain).unwrap_or(&none);
        let since_ok = watch.last_ok_at.unwrap_or(tracker.started_at);
        let stale = at.saturating_sub(since_ok) > FAILING_GRACE;
        let failing = watch
            .failing_since
            .is_some_and(|s| at.saturating_sub(s) > FAILING_GRACE);
        if stale || failing {
            out.push(Condition {
                key: format!("watch-failing:{chain}"),
                severity: Severity::Critical,
                subject: Subject::Chain,
                subject_id: chain.to_string(),
                title: format!("Cannot watch {chain}"),
                detail: format!(
                    "new transfers from {chain} may be stuck without being noticed: {}",
                    watch
                        .error
                        .as_deref()
                        .unwrap_or("no successful read recently")
                ),
            });
        }
    }
    out
}

/// A notification that opened or resolved in one sweep, for whoever relays them (Slack).
#[derive(Debug, Clone)]
pub enum Change {
    Opened(Notification),
    Resolved(Notification),
}

/// Bring the inbox up to date with the conditions that hold now, and say what changed.
pub fn sweep(tracker: &Tracker) -> Vec<Change> {
    let mut changes = Vec::new();
    let at = now();
    let current = conditions(tracker, at);
    let mut state = tracker.lock();
    let inbox = &mut state.inbox;
    for c in &current {
        match inbox
            .iter_mut()
            .find(|n| n.key == c.key && n.resolved_at.is_none())
        {
            Some(open) => {
                open.updated_at = at;
                open.detail = c.detail.clone();
                open.title = c.title.clone();
                open.severity = open.severity.max(c.severity);
            }
            None => {
                match c.severity {
                    Severity::Critical => tracing::error!("PROBLEM {}: {}", c.title, c.detail),
                    Severity::Warning => tracing::warn!("problem {}: {}", c.title, c.detail),
                }
                let opened = Notification {
                    id: format!("{}@{at}", c.key),
                    key: c.key.clone(),
                    severity: c.severity,
                    subject: c.subject,
                    subject_id: c.subject_id.clone(),
                    title: c.title.clone(),
                    detail: c.detail.clone(),
                    opened_at: at,
                    updated_at: at,
                    resolved_at: None,
                };
                changes.push(Change::Opened(opened.clone()));
                inbox.push(opened);
            }
        }
    }
    for n in inbox.iter_mut().filter(|n| n.resolved_at.is_none()) {
        if !current.iter().any(|c| c.key == n.key) {
            tracing::info!(
                "resolved after {}: {}",
                duration(at.saturating_sub(n.opened_at)),
                n.title
            );
            n.resolved_at = Some(at);
            changes.push(Change::Resolved(n.clone()));
        }
    }
    inbox.retain(|n| {
        n.resolved_at
            .is_none_or(|r| at.saturating_sub(r) < KEEP_RESOLVED)
    });
    state.last_sweep_at = Some(at);
    changes
}

/// How often the log gets a status line, so a quiet log still says the relayer is alive.
pub const SUMMARY_EVERY: u64 = 5 * 60;

/// How everything is, right now: what the status line and the Slack status post both say.
pub struct Summary {
    pub routes: usize,
    pub in_flight: usize,
    pub delivered_last_hour: usize,
    /// Open notifications, most severe first.
    pub open: Vec<Notification>,
    /// The gas wallet closest to empty, as (chain, days left), when known.
    pub lowest_gas: Option<(String, f64)>,
}

impl Summary {
    pub fn critical(&self) -> bool {
        self.open.iter().any(|n| n.severity == Severity::Critical)
    }
}

pub fn summary(tracker: &Tracker) -> Summary {
    let at = now();
    let state = tracker.lock();
    let mut open: Vec<Notification> = state
        .inbox
        .iter()
        .filter(|n| n.resolved_at.is_none())
        .cloned()
        .collect();
    open.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(a.opened_at.cmp(&b.opened_at))
    });
    let mut in_flight = 0;
    let mut delivered_last_hour = 0;
    for m in state.messages.values() {
        match &m.delivery {
            None => in_flight += 1,
            Some(d) if at.saturating_sub(d.at) < 3600 => delivered_last_hour += 1,
            Some(_) => {}
        }
    }
    let lowest_gas = tracker
        .wallets
        .iter()
        .filter_map(|w| {
            let r = state.wallets.get(&w.chain)?;
            let (_, days, _) = crate::wallets::judge(w, r);
            Some((w.chain.clone(), days?))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1));
    Summary {
        routes: tracker.routes.len(),
        in_flight,
        delivered_last_hour,
        open,
        lowest_gas,
    }
}

/// One line on how everything is: all good, or which problems are open.
pub fn log_summary(tracker: &Tracker) {
    let s = summary(tracker);
    let (in_flight, delivered) = (s.in_flight, s.delivered_last_hour);
    if s.open.is_empty() {
        tracing::info!(
            "all good: {} routes healthy, {in_flight} transfer{} in flight, {delivered} delivered in the last hour",
            s.routes,
            if in_flight == 1 { "" } else { "s" }
        );
        return;
    }
    let titles: Vec<&str> = s.open.iter().map(|n| n.title.as_str()).collect();
    let line = format!(
        "{} problem{} open: {}; {in_flight} in flight, {delivered} delivered in the last hour; details at /api/v1/inbox",
        s.open.len(),
        if s.open.len() == 1 { "" } else { "s" },
        titles.join("; ")
    );
    if s.critical() {
        tracing::error!("{line}");
    } else {
        tracing::warn!("{line}");
    }
}

/// The monitor's own health, which it cannot report through the inbox: if the sweep stops,
/// nothing new would ever open. Checked when the API answers.
pub fn monitor_problem(tracker: &Tracker, at: u64) -> Option<Notification> {
    let last = tracker.lock().last_sweep_at;
    let limit = 3 * tracker.track_secs + 60;
    let silent = last.is_none_or(|t| at.saturating_sub(t) > limit);
    silent.then(|| Notification {
        id: "monitor-silent".into(),
        key: "monitor-silent".into(),
        severity: Severity::Critical,
        subject: Subject::Monitor,
        subject_id: "monitor".into(),
        title: "The monitor has stopped".into(),
        detail: format!(
            "no check has run since {}; the inbox is not being updated",
            last.map_or("startup".into(), |t| t.to_string())
        ),
        opened_at: last.unwrap_or(at),
        updated_at: at,
        resolved_at: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::{Dispatch, Verified};

    struct NoDestination;
    #[async_trait::async_trait]
    impl crate::destination::Destination for NoDestination {
        async fn state(&self) -> anyhow::Result<tee_node::state::IsmState> {
            anyhow::bail!("unused")
        }
        async fn submit(
            &self,
            _: &crate::destination::Batch,
        ) -> anyhow::Result<Vec<crate::destination::Delivery>> {
            anyhow::bail!("unused")
        }
        async fn deliver(&self, _: &[u8]) -> anyhow::Result<crate::destination::Outcome> {
            anyhow::bail!("unused")
        }
        async fn delivered(&self, _: &str) -> anyhow::Result<bool> {
            anyhow::bail!("unused")
        }
        async fn wallet(&self) -> anyhow::Result<crate::destination::Wallet> {
            anyhow::bail!("unused")
        }
    }

    fn route(latency: u64) -> TrackedRoute {
        TrackedRoute {
            name: "a-to-b".into(),
            from: "a".into(),
            to: "b".into(),
            origin_domain: 1,
            destination_domain: 2,
            ism: String::new(),
            routers: vec![],
            expected_latency: latency,
            dir: std::env::temp_dir(),
            destination: Box::new(NoDestination),
        }
    }

    fn record(dispatched: u64) -> MessageRecord {
        MessageRecord {
            id: "0x01".into(),
            route: "a-to-b".into(),
            nonce: 0,
            sender: String::new(),
            recipient: String::new(),
            transfer: None,
            dispatch: Some(Dispatch {
                tx: "0xaa".into(),
                block: 10,
                timestamp: dispatched,
                from: None,
            }),
            first_seen_at: dispatched,
            attestable_at: None,
            verified: None,
            delivery: None,
        }
    }

    #[test]
    fn a_fresh_transfer_is_pending_and_an_old_one_overdue() {
        let r = route(600);
        assert_eq!(
            assess(&record(1000), &r, "celestia", None, 1100).status,
            Status::Pending
        );
        let late = assess(&record(1000), &r, "celestia", None, 1601);
        assert_eq!(late.status, Status::Overdue);
        assert!(late.problem.unwrap().contains("not verified"));
    }

    /// Base takes five days, but once its anchor covers the block the ISM must follow within
    /// minutes, and a transfer stuck behind that is reported long before the five days.
    #[test]
    fn attestable_but_not_verified_is_stuck_well_before_the_backstop() {
        let r = route(5 * 86400);
        let mut m = record(0);
        m.attestable_at = Some(1000);
        assert_eq!(
            assess(&m, &r, "base", None, 1000 + ATTEST_GRACE).status,
            Status::Pending
        );
        assert_eq!(
            assess(&m, &r, "base", None, 1001 + ATTEST_GRACE).status,
            Status::Overdue
        );
    }

    #[test]
    fn verified_but_undelivered_is_stuck_after_the_grace() {
        let r = route(5 * 86400);
        let mut m = record(0);
        m.verified = Some(Verified {
            at: 50,
            ism_height: 10,
        });
        assert_eq!(
            assess(&m, &r, "base", None, 50 + DELIVERY_GRACE).status,
            Status::Verified
        );
        assert_eq!(
            assess(&m, &r, "base", None, 51 + DELIVERY_GRACE).status,
            Status::Overdue
        );
    }

    #[test]
    fn a_parked_transfer_is_failed_and_a_delivered_one_is_done() {
        let r = route(600);
        let p = Parked {
            id: "01".into(),
            message: String::new(),
            attempts: 2,
            first_failed_at: 5,
            last_attempt_at: 6,
            next_attempt_at: 7,
            reason: "recipient reverted".into(),
            tx: None,
        };
        let a = assess(&record(0), &r, "celestia", Some(&p), 10);
        assert_eq!(a.status, Status::Failed);
        assert!(a.problem.unwrap().contains("recipient reverted"));
        let mut m = record(0);
        m.delivery = Some(crate::tracker::DeliveryInfo { at: 9, tx: None });
        assert_eq!(
            assess(&m, &r, "celestia", Some(&p), 1_000_000).status,
            Status::Delivered
        );
    }
}
