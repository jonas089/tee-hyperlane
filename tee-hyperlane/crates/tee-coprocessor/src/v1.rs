//! `/api/v1`: the explorer's API, and the contract anything monitoring the bridge builds on.
//!
//! Every response type is here, and `ui/openapi.json` describes each one. A test holds the two
//! to each other, so a field cannot change or disappear without the spec saying so. Times are
//! unix seconds throughout; message ids and 32-byte addresses are 0x-prefixed lowercase hex.
//!
//! Read-only, and there is nothing to authenticate: every value is public chain data or a
//! report about it. In particular a notification cannot be dismissed here, only resolved by its
//! problem going away, so no caller can hide a problem from anyone else.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::monitor::{self, Assessment, Notification, Severity, Status, Subject};
use crate::route::{now, Parked};
use crate::tracker::{route_head, IsmReading, MessageRecord, TrackedRoute, Tracker, Watch};

pub const SPEC: &str = include_str!("../ui/openapi.json");

pub fn router(tracker: Arc<Tracker>) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/inbox", get(inbox))
        .route("/api/v1/routes", get(routes))
        .route("/api/v1/routes/{name}", get(route))
        .route("/api/v1/chains", get(chains))
        .route("/api/v1/wallets", get(wallets))
        .route("/api/v1/messages", get(messages))
        .route("/api/v1/messages/{id}", get(message))
        .route("/api/v1/search", get(search))
        .route(
            "/api/v1/openapi.json",
            get(|| async { ([(header::CONTENT_TYPE, "application/json")], SPEC) }),
        )
        .route("/metrics", get(metrics))
        .with_state(tracker)
}

// ---------------------------------------------------------------- types

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Ok,
    Degraded,
    Down,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthView {
    pub status: Health,
    pub checked_at: u64,
    pub last_sweep_at: Option<u64>,
    pub open: SeverityCounts,
}

#[derive(Debug, Default, Serialize)]
pub struct SeverityCounts {
    pub critical: usize,
    pub warning: usize,
}

#[derive(Debug, Serialize)]
pub struct InboxView {
    /// Open first, most severe first, newest first; then resolved ones, newest first.
    pub notifications: Vec<Notification>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChainRef {
    pub name: String,
    pub domain: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainView {
    pub name: String,
    pub kind: String,
    pub domain: u32,
    pub explorer: Option<String>,
    /// The tracker's watch on it, when it is an origin.
    pub watch: Option<Watch>,
}

#[derive(Debug, Default, Serialize)]
pub struct StatusCounts {
    pub pending: usize,
    pub verified: usize,
    pub delivered: usize,
    pub failed: usize,
    pub overdue: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginHead {
    pub height: u64,
    pub reported_at: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failing {
    pub error: String,
    pub consecutive_failures: u64,
    pub since: Option<u64>,
    pub at: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Staged {
    pub height: Option<u64>,
    pub messages: usize,
    pub since: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteView {
    pub name: String,
    pub from: ChainRef,
    pub to: ChainRef,
    pub ism: String,
    pub status: Health,
    pub expected_latency_secs: u64,
    pub ism_reading: IsmReading,
    /// How far the route loop last found the origin could go.
    pub origin_head: Option<OriginHead>,
    pub failing: Option<Failing>,
    pub staged: Option<Staged>,
    pub counts: StatusCounts,
    pub notifications: Vec<Notification>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub attempts: u32,
    pub first_failed_at: u64,
    pub last_attempt_at: u64,
    pub next_attempt_at: u64,
    pub reason: String,
    pub tx: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Links {
    pub dispatch_tx: Option<String>,
    pub delivery_tx: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageView {
    #[serde(flatten)]
    pub record: MessageRecord,
    pub origin: ChainRef,
    pub destination: ChainRef,
    #[serde(flatten)]
    pub assessment: Assessment,
    pub failure: Option<Failure>,
    pub links: Links,
    pub notifications: Vec<Notification>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletView {
    pub chain: String,
    pub address: Option<String>,
    pub symbol: String,
    pub decimals: u32,
    /// Base units, decimal.
    pub balance: Option<String>,
    /// The same, in whole tokens, rounded for display.
    pub display: Option<String>,
    /// The warning level, in whole tokens.
    pub low_at: f64,
    /// Whole tokens spent per day over the last week, when there is enough history to say.
    pub spent_per_day: Option<f64>,
    pub days_left: Option<f64>,
    pub level: crate::wallets::Level,
    pub problem: Option<String>,
    pub checked_at: Option<u64>,
    pub error: Option<String>,
    pub explorer: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MessagePage {
    pub total: usize,
    pub messages: Vec<MessageView>,
}

#[derive(Debug, Serialize)]
pub struct SearchView {
    pub query: String,
    /// What the query was read as: `message`, `transaction`, `account`, or `none`.
    pub matched: Vec<String>,
    pub messages: Vec<MessageView>,
}

// ---------------------------------------------------------------- building views

/// Every open notification, including the monitor's own when it has stopped.
fn open_notifications(tracker: &Tracker, at: u64) -> Vec<Notification> {
    let mut open: Vec<Notification> = tracker
        .lock()
        .inbox
        .iter()
        .filter(|n| n.resolved_at.is_none())
        .cloned()
        .collect();
    open.extend(monitor::monitor_problem(tracker, at));
    open
}

fn chain_ref(tracker: &Tracker, name: &str, domain: u32) -> ChainRef {
    ChainRef {
        name: tracker
            .chain(name)
            .map_or(name, |c| c.name.as_str())
            .to_string(),
        domain,
    }
}

fn message_view(
    tracker: &Tracker,
    record: &MessageRecord,
    open: &[Notification],
    at: u64,
) -> Option<MessageView> {
    let route = tracker.route(&record.route)?;
    let parked = tracker.parked(route, &record.id);
    let origin_kind = tracker.chain(&route.from).map_or("", |c| c.kind.as_str());
    let assessment = monitor::assess(record, route, origin_kind, parked.as_ref(), at);
    let origin = tracker.chain(&route.from);
    let destination = tracker.chain(&route.to);
    Some(MessageView {
        origin: chain_ref(tracker, &route.from, route.origin_domain),
        destination: chain_ref(tracker, &route.to, route.destination_domain),
        assessment,
        failure: parked.map(failure),
        links: Links {
            dispatch_tx: record
                .dispatch
                .as_ref()
                .and_then(|d| origin.and_then(|c| c.tx_url(&d.tx))),
            delivery_tx: record
                .delivery
                .as_ref()
                .and_then(|d| d.tx.as_ref())
                .and_then(|tx| destination.and_then(|c| c.tx_url(tx))),
        },
        notifications: open
            .iter()
            .filter(|n| n.subject == Subject::Message && n.subject_id == record.id)
            .cloned()
            .collect(),
        record: record.clone(),
    })
}

fn failure(p: Parked) -> Failure {
    Failure {
        attempts: p.attempts,
        first_failed_at: p.first_failed_at,
        last_attempt_at: p.last_attempt_at,
        next_attempt_at: p.next_attempt_at,
        reason: p.reason,
        tx: p.tx,
    }
}

/// A transfer the tracker has no record of but the route has parked, so a stuck message is
/// reported even when the tracker missed its dispatch.
fn parked_only(
    tracker: &Tracker,
    route: &TrackedRoute,
    p: &Parked,
    open: &[Notification],
) -> Option<MessageView> {
    let id = format!("0x{}", p.id.trim_start_matches("0x"));
    let bytes = hex::decode(&p.message).unwrap_or_default();
    // Listed even when its bytes do not decode: a stuck message must never drop out of view.
    let decoded = hyperlane_types::decode_hyperlane_message(&bytes).ok();
    let record = MessageRecord {
        id: id.clone(),
        route: route.name.clone(),
        nonce: decoded.as_ref().map_or(0, |d| d.nonce),
        sender: decoded
            .as_ref()
            .map_or_else(String::new, |d| format!("0x{}", hex::encode(d.sender))),
        recipient: decoded
            .as_ref()
            .map_or_else(String::new, |d| format!("0x{}", hex::encode(d.recipient))),
        transfer: None,
        dispatch: None,
        first_seen_at: p.first_failed_at,
        attestable_at: None,
        verified: None,
        delivery: None,
    };
    message_view(tracker, &record, open, now())
}

fn all_messages(tracker: &Tracker, open: &[Notification], at: u64) -> Vec<MessageView> {
    let records: Vec<MessageRecord> = tracker.lock().messages.values().cloned().collect();
    let mut views: Vec<MessageView> = records
        .iter()
        .filter_map(|r| message_view(tracker, r, open, at))
        .collect();
    for route in &tracker.routes {
        for p in tracker.parked_all(route) {
            let id = format!("0x{}", p.id.trim_start_matches("0x"));
            if !views.iter().any(|v| v.record.id == id) {
                views.extend(parked_only(tracker, route, &p, open));
            }
        }
    }
    views
}

fn rank(status: Status) -> u8 {
    match status {
        Status::Overdue => 0,
        Status::Failed => 1,
        Status::Pending => 2,
        Status::Verified => 3,
        Status::Delivered => 4,
    }
}

/// Problems first, then newest first.
fn sort_messages(views: &mut [MessageView]) {
    views.sort_by(|a, b| {
        rank(a.assessment.status)
            .cmp(&rank(b.assessment.status))
            .then(dispatched(b).cmp(&dispatched(a)))
    });
}

fn dispatched(v: &MessageView) -> u64 {
    v.record
        .dispatch
        .as_ref()
        .map_or(v.record.first_seen_at, |d| d.timestamp)
}

fn route_view(
    tracker: &Tracker,
    route: &TrackedRoute,
    views: &[MessageView],
    open: &[Notification],
) -> RouteView {
    let mut counts = StatusCounts::default();
    for v in views.iter().filter(|v| v.record.route == route.name) {
        match v.assessment.status {
            Status::Pending => counts.pending += 1,
            Status::Verified => counts.verified += 1,
            Status::Delivered => counts.delivered += 1,
            Status::Failed => counts.failed += 1,
            Status::Overdue => counts.overdue += 1,
        }
    }
    let notifications: Vec<Notification> = open
        .iter()
        .filter(|n| match n.subject {
            Subject::Route => n.subject_id == route.name,
            Subject::Message => views
                .iter()
                .any(|v| v.record.id == n.subject_id && v.record.route == route.name),
            // A gas wallet pays for delivering to its chain; a watch reads transfers from it.
            Subject::Chain if n.key.starts_with("wallet-") => n.subject_id == route.to,
            Subject::Chain => n.subject_id == route.from,
            Subject::Monitor => true,
        })
        .cloned()
        .collect();
    let blocked: Option<serde_json::Value> =
        crate::tracker::read_json(&route.dir.join("blocked.json"));
    let staged: Option<serde_json::Value> =
        crate::tracker::read_json(&route.dir.join("staging").join("batch.json"));
    let staged_since = std::fs::metadata(route.dir.join("staging").join("batch.json"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    RouteView {
        name: route.name.clone(),
        from: chain_ref(tracker, &route.from, route.origin_domain),
        to: chain_ref(tracker, &route.to, route.destination_domain),
        ism: route.ism.clone(),
        status: health_of(&notifications),
        expected_latency_secs: route.expected_latency,
        ism_reading: tracker
            .lock()
            .isms
            .get(&route.name)
            .cloned()
            .unwrap_or_default(),
        origin_head: route_head(&route.dir).map(|(height, reported_at)| OriginHead {
            height,
            reported_at,
        }),
        failing: blocked.map(|b| Failing {
            error: b["error"].as_str().unwrap_or("").to_string(),
            consecutive_failures: b["consecutiveFailures"].as_u64().unwrap_or(0),
            since: b["since"].as_u64(),
            at: b["at"].as_u64(),
        }),
        staged: staged.map(|s| Staged {
            height: s["attestation"]["new_state"]
                .as_str()
                .and_then(|h| hex::decode(h).ok())
                .and_then(|b| tee_node::state::IsmState::decode(&b).ok())
                .map(|st| st.height),
            messages: s["messages"].as_array().map_or(0, |m| m.len()),
            since: staged_since,
        }),
        counts,
        notifications,
    }
}

/// Down on any critical problem, degraded on any warning.
fn health_of(open: &[Notification]) -> Health {
    if open.iter().any(|n| n.severity == Severity::Critical) {
        Health::Down
    } else if open.is_empty() {
        Health::Ok
    } else {
        Health::Degraded
    }
}

/// What a search string can mean: hex without its prefix, lowercased, and the account bytes it
/// names if it is an address.
struct Needle {
    hex: Option<String>,
    account: Option<Vec<u8>>,
}

fn parse_query(q: &str) -> Needle {
    let q = q.trim();
    if let Some((_, bytes)) = crate::bech32::decode(q) {
        return Needle {
            hex: None,
            account: Some(bytes),
        };
    }
    let bare = q.trim_start_matches("0x").to_lowercase();
    if bare.is_empty() || !bare.chars().all(|c| c.is_ascii_hexdigit()) {
        return Needle {
            hex: None,
            account: None,
        };
    }
    let account = match bare.len() {
        40 => hex::decode(&bare).ok(),
        // A 32-byte value is also an account when it is a padded 20-byte one.
        64 if bare.starts_with(&"0".repeat(24)) => hex::decode(&bare[24..]).ok(),
        64 => hex::decode(&bare).ok(),
        _ => None,
    };
    Needle {
        hex: Some(bare),
        account,
    }
}

/// An account as bytes, from how a record writes it.
fn account_bytes(text: &str) -> Option<Vec<u8>> {
    if let Some((_, bytes)) = crate::bech32::decode(text) {
        return Some(bytes);
    }
    let raw = hex::decode(text.trim_start_matches("0x")).ok()?;
    if raw.len() == 32 && raw[..12].iter().all(|b| *b == 0) {
        return Some(raw[12..].to_vec());
    }
    Some(raw)
}

fn matches(record: &MessageRecord, needle: &Needle) -> Vec<&'static str> {
    let mut how = Vec::new();
    let bare = |s: &str| s.trim_start_matches("0x").to_lowercase();
    if let Some(h) = &needle.hex {
        if bare(&record.id) == *h {
            how.push("message");
        }
        let txs = [
            record.dispatch.as_ref().map(|d| d.tx.as_str()),
            record.delivery.as_ref().and_then(|d| d.tx.as_deref()),
        ];
        if txs.into_iter().flatten().any(|t| bare(t) == *h) {
            how.push("transaction");
        }
    }
    if let Some(account) = &needle.account {
        let accounts = [
            record.dispatch.as_ref().and_then(|d| d.from.as_deref()),
            record.transfer.as_ref().map(|t| t.recipient.as_str()),
            record.transfer.as_ref().map(|t| t.recipient_hex.as_str()),
        ];
        if accounts
            .into_iter()
            .flatten()
            .filter_map(account_bytes)
            .any(|a| a == *account)
        {
            how.push("account");
        }
    }
    how
}

// ---------------------------------------------------------------- handlers

async fn health(State(tracker): State<Arc<Tracker>>) -> impl IntoResponse {
    let at = now();
    let open = open_notifications(&tracker, at);
    let mut counts = SeverityCounts::default();
    for n in &open {
        match n.severity {
            Severity::Critical => counts.critical += 1,
            Severity::Warning => counts.warning += 1,
        }
    }
    let status = health_of(&open);
    let view = HealthView {
        status,
        checked_at: at,
        last_sweep_at: tracker.lock().last_sweep_at,
        open: counts,
    };
    // Anything short of ok answers 503, so a plain uptime probe alerts on it.
    let code = if status == Health::Ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(view))
}

#[derive(Deserialize)]
struct InboxQuery {
    #[serde(default)]
    resolved: bool,
}

async fn inbox(
    State(tracker): State<Arc<Tracker>>,
    Query(q): Query<InboxQuery>,
) -> Json<InboxView> {
    let at = now();
    let mut notifications = open_notifications(&tracker, at);
    notifications.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.opened_at.cmp(&a.opened_at))
    });
    if q.resolved {
        let mut resolved: Vec<Notification> = tracker
            .lock()
            .inbox
            .iter()
            .filter(|n| n.resolved_at.is_some())
            .cloned()
            .collect();
        resolved.sort_by_key(|n| std::cmp::Reverse(n.resolved_at));
        notifications.extend(resolved);
    }
    Json(InboxView { notifications })
}

async fn routes(State(tracker): State<Arc<Tracker>>) -> Json<Vec<RouteView>> {
    let at = now();
    let open = open_notifications(&tracker, at);
    let views = all_messages(&tracker, &open, at);
    Json(
        tracker
            .routes
            .iter()
            .map(|r| route_view(&tracker, r, &views, &open))
            .collect(),
    )
}

async fn route(
    State(tracker): State<Arc<Tracker>>,
    Path(name): Path<String>,
) -> Result<Json<RouteView>, StatusCode> {
    let at = now();
    let open = open_notifications(&tracker, at);
    let views = all_messages(&tracker, &open, at);
    let r = tracker.route(&name).ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(route_view(&tracker, r, &views, &open)))
}

async fn chains(State(tracker): State<Arc<Tracker>>) -> Json<Vec<ChainView>> {
    let watches = tracker.lock().watches.clone();
    Json(
        tracker
            .chains
            .values()
            .map(|c| ChainView {
                name: c.name.clone(),
                kind: c.kind.clone(),
                domain: c.domain,
                explorer: c.explorer.clone(),
                watch: watches.get(&c.name).cloned(),
            })
            .collect(),
    )
}

fn wallet_views(tracker: &Tracker) -> Vec<WalletView> {
    let readings = tracker.lock().wallets.clone();
    tracker
        .wallets
        .iter()
        .map(|w| {
            let r = readings.get(&w.chain).cloned().unwrap_or_default();
            let (level, days_left, problem) = crate::wallets::judge(w, &r);
            let balance = r.balance.as_ref().and_then(|b| b.parse::<u128>().ok());
            WalletView {
                chain: w.chain.clone(),
                symbol: w.symbol.clone(),
                decimals: w.decimals,
                display: balance.map(|b| crate::wallets::show(b, w.decimals)),
                low_at: w.low,
                spent_per_day: r.spend_per_day().map(|s| s / 10f64.powi(w.decimals as i32)),
                days_left,
                level,
                problem,
                checked_at: r.checked_at,
                error: r.error.clone(),
                explorer: tracker.chain(&w.chain).and_then(|c| {
                    let a = r.address.as_ref()?;
                    c.explorer
                        .as_ref()
                        .map(|e| format!("{}/address/{a}", e.trim_end_matches('/')))
                }),
                address: r.address,
                balance: r.balance,
            }
        })
        .collect()
}

async fn wallets(State(tracker): State<Arc<Tracker>>) -> Json<Vec<WalletView>> {
    Json(wallet_views(&tracker))
}

#[derive(Deserialize)]
struct MessageQuery {
    status: Option<String>,
    route: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

async fn messages(
    State(tracker): State<Arc<Tracker>>,
    Query(q): Query<MessageQuery>,
) -> Result<Json<MessagePage>, (StatusCode, String)> {
    let wanted: Option<Vec<Status>> = match &q.status {
        None => None,
        Some(s) => Some(
            s.split(',')
                .map(|one| {
                    serde_json::from_value(serde_json::Value::String(one.trim().to_string()))
                        .map_err(|_| (StatusCode::BAD_REQUEST, format!("unknown status `{one}`")))
                })
                .collect::<Result<_, _>>()?,
        ),
    };
    let at = now();
    let open = open_notifications(&tracker, at);
    let mut views: Vec<MessageView> = all_messages(&tracker, &open, at)
        .into_iter()
        .filter(|v| q.route.as_ref().is_none_or(|r| &v.record.route == r))
        .filter(|v| {
            wanted
                .as_ref()
                .is_none_or(|w| w.contains(&v.assessment.status))
        })
        .collect();
    sort_messages(&mut views);
    let total = views.len();
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let messages = views
        .into_iter()
        .skip(q.offset.unwrap_or(0))
        .take(limit)
        .collect();
    Ok(Json(MessagePage { total, messages }))
}

async fn message(
    State(tracker): State<Arc<Tracker>>,
    Path(id): Path<String>,
) -> Result<Json<MessageView>, StatusCode> {
    let wanted = format!("0x{}", id.trim_start_matches("0x").to_lowercase());
    let at = now();
    let open = open_notifications(&tracker, at);
    all_messages(&tracker, &open, at)
        .into_iter()
        .find(|v| v.record.id == wanted)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

async fn search(
    State(tracker): State<Arc<Tracker>>,
    Query(q): Query<SearchQuery>,
) -> Json<SearchView> {
    let needle = parse_query(&q.q);
    let at = now();
    let open = open_notifications(&tracker, at);
    let mut matched: Vec<String> = Vec::new();
    let mut found: Vec<MessageView> = Vec::new();
    for view in all_messages(&tracker, &open, at) {
        let how = matches(&view.record, &needle);
        if how.is_empty() {
            continue;
        }
        for h in how {
            if !matched.iter().any(|m| m == h) {
                matched.push(h.to_string());
            }
        }
        found.push(view);
    }
    sort_messages(&mut found);
    if matched.is_empty() {
        matched.push("none".into());
    }
    Json(SearchView {
        query: q.q,
        matched,
        messages: found,
    })
}

/// Prometheus text format, for alerting on the same verdicts the inbox shows.
async fn metrics(State(tracker): State<Arc<Tracker>>) -> impl IntoResponse {
    let at = now();
    let open = open_notifications(&tracker, at);
    let views = all_messages(&tracker, &open, at);
    let mut out = String::new();
    out.push_str("# HELP teeism_notifications_open Open inbox notifications.\n");
    out.push_str("# TYPE teeism_notifications_open gauge\n");
    for (label, severity) in [
        ("critical", Severity::Critical),
        ("warning", Severity::Warning),
    ] {
        let n = open.iter().filter(|n| n.severity == severity).count();
        out.push_str(&format!(
            "teeism_notifications_open{{severity=\"{label}\"}} {n}\n"
        ));
    }
    out.push_str("# HELP teeism_messages Tracked transfers by route and status.\n");
    out.push_str("# TYPE teeism_messages gauge\n");
    let mut counts: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    for route in &tracker.routes {
        for s in ["pending", "verified", "delivered", "failed", "overdue"] {
            counts.insert((route.name.clone(), s), 0);
        }
    }
    for v in &views {
        let s = match v.assessment.status {
            Status::Pending => "pending",
            Status::Verified => "verified",
            Status::Delivered => "delivered",
            Status::Failed => "failed",
            Status::Overdue => "overdue",
        };
        *counts.entry((v.record.route.clone(), s)).or_default() += 1;
    }
    for ((route, status), n) in counts {
        out.push_str(&format!(
            "teeism_messages{{route=\"{route}\",status=\"{status}\"}} {n}\n"
        ));
    }
    out.push_str("# HELP teeism_ism_height The ISM's trusted origin height, per route.\n");
    out.push_str("# TYPE teeism_ism_height gauge\n");
    for (route, ism) in tracker.lock().isms.iter() {
        if let Some(h) = ism.height {
            out.push_str(&format!("teeism_ism_height{{route=\"{route}\"}} {h}\n"));
        }
    }
    out.push_str(
        "# HELP teeism_wallet_balance The relayer's gas balance per chain, in whole tokens.\n",
    );
    out.push_str("# TYPE teeism_wallet_balance gauge\n");
    out.push_str("# HELP teeism_wallet_days_left Days of gas left at the last week's spend.\n");
    out.push_str("# TYPE teeism_wallet_days_left gauge\n");
    for w in wallet_views(&tracker) {
        if let Some(b) = w.balance.as_ref().and_then(|b| b.parse::<u128>().ok()) {
            out.push_str(&format!(
                "teeism_wallet_balance{{chain=\"{}\"}} {}\n",
                w.chain,
                crate::wallets::whole(b, w.decimals)
            ));
        }
        if let Some(days) = w.days_left {
            out.push_str(&format!(
                "teeism_wallet_days_left{{chain=\"{}\"}} {days:.2}\n",
                w.chain
            ));
        }
    }
    out.push_str("# HELP teeism_last_sweep_seconds When the monitor last ran.\n");
    out.push_str("# TYPE teeism_last_sweep_seconds gauge\n");
    out.push_str(&format!(
        "teeism_last_sweep_seconds {}\n",
        tracker.lock().last_sweep_at.unwrap_or(0)
    ));
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_is_read_every_way_it_can_mean() {
        let tx = parse_query("0xABCDEF0000000000000000000000000000000000000000000000000000000001");
        assert_eq!(
            tx.hex.as_deref(),
            Some("abcdef0000000000000000000000000000000000000000000000000000000001")
        );
        let evm = parse_query("0x9822eE81C82138F88D759faef1AC168aDfEe1467");
        assert_eq!(evm.account.unwrap().len(), 20);
        let padded = parse_query(&format!("0x{}{}", "0".repeat(24), "ab".repeat(20)));
        assert_eq!(padded.account.unwrap(), vec![0xab; 20]);
        let celestia = crate::bech32::encode("celestia", &[7u8; 20]);
        assert_eq!(parse_query(&celestia).account.unwrap(), vec![7u8; 20]);
        assert!(parse_query("not a thing").account.is_none());
    }

    /// The spec lists every field each response carries, and marks as required every field
    /// that is always present. A field added, renamed or dropped without the spec fails here.
    #[test]
    fn responses_match_the_published_spec() {
        let spec: serde_json::Value = serde_json::from_str(SPEC).unwrap();
        let schemas = &spec["components"]["schemas"];
        let check = |name: &str, value: serde_json::Value| {
            let schema = &schemas[name];
            let props = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("no schema {name}"));
            let object = value.as_object().unwrap();
            for key in object.keys() {
                assert!(
                    props.contains_key(key),
                    "{name}.{key} is served but not in the spec"
                );
            }
            for key in props.keys() {
                assert!(
                    object.contains_key(key),
                    "{name}.{key} is in the spec but not served"
                );
            }
        };
        let record = MessageRecord {
            id: "0x01".into(),
            route: "r".into(),
            nonce: 1,
            sender: "0x".into(),
            recipient: "0x".into(),
            transfer: Some(crate::tracker::Transfer {
                recipient: "celestia1".into(),
                recipient_hex: "0x".into(),
                amount: "1".into(),
            }),
            dispatch: Some(crate::tracker::Dispatch {
                tx: "0x".into(),
                block: 1,
                timestamp: 1,
                from: Some("0x".into()),
            }),
            first_seen_at: 1,
            attestable_at: Some(1),
            verified: Some(crate::tracker::Verified {
                at: 1,
                ism_height: 1,
            }),
            delivery: Some(crate::tracker::DeliveryInfo {
                at: 1,
                tx: Some("0x".into()),
            }),
        };
        let notification = Notification {
            id: "k@1".into(),
            key: "k".into(),
            severity: Severity::Critical,
            subject: Subject::Message,
            subject_id: "0x01".into(),
            title: "t".into(),
            detail: "d".into(),
            opened_at: 1,
            updated_at: 1,
            resolved_at: None,
        };
        let view = MessageView {
            record: record.clone(),
            origin: ChainRef {
                name: "a".into(),
                domain: 1,
            },
            destination: ChainRef {
                name: "b".into(),
                domain: 2,
            },
            assessment: Assessment {
                status: Status::Delivered,
                expected_by: 1,
                waiting_on: None,
                problem: None,
            },
            failure: Some(Failure {
                attempts: 1,
                first_failed_at: 1,
                last_attempt_at: 1,
                next_attempt_at: 1,
                reason: "r".into(),
                tx: None,
            }),
            links: Links {
                dispatch_tx: None,
                delivery_tx: None,
            },
            notifications: vec![notification.clone()],
        };
        let v = serde_json::to_value(&view).unwrap();
        check("Message", v.clone());
        check("Transfer", v["transfer"].clone());
        check("Dispatch", v["dispatch"].clone());
        check("Verified", v["verified"].clone());
        check("Delivery", v["delivery"].clone());
        check("Failure", v["failure"].clone());
        check("Links", v["links"].clone());
        check("ChainRef", v["origin"].clone());
        check("Notification", serde_json::to_value(&notification).unwrap());
        check(
            "Health",
            serde_json::to_value(HealthView {
                status: Health::Ok,
                checked_at: 1,
                last_sweep_at: Some(1),
                open: SeverityCounts::default(),
            })
            .unwrap(),
        );
        let route = RouteView {
            name: "r".into(),
            from: ChainRef {
                name: "a".into(),
                domain: 1,
            },
            to: ChainRef {
                name: "b".into(),
                domain: 2,
            },
            ism: "0x".into(),
            status: Health::Ok,
            expected_latency_secs: 1,
            ism_reading: IsmReading::default(),
            origin_head: Some(OriginHead {
                height: 1,
                reported_at: 1,
            }),
            failing: Some(Failing {
                error: "e".into(),
                consecutive_failures: 1,
                since: Some(1),
                at: Some(1),
            }),
            staged: Some(Staged {
                height: Some(1),
                messages: 1,
                since: Some(1),
            }),
            counts: StatusCounts::default(),
            notifications: vec![],
        };
        let r = serde_json::to_value(&route).unwrap();
        check("Route", r.clone());
        check("IsmReading", r["ismReading"].clone());
        check("OriginHead", r["originHead"].clone());
        check("Failing", r["failing"].clone());
        check("Staged", r["staged"].clone());
        check("StatusCounts", r["counts"].clone());
        check(
            "Chain",
            serde_json::to_value(ChainView {
                name: "a".into(),
                kind: "k".into(),
                domain: 1,
                explorer: None,
                watch: Some(Watch::default()),
            })
            .unwrap(),
        );
        check("Watch", serde_json::to_value(Watch::default()).unwrap());
        check(
            "Wallet",
            serde_json::to_value(WalletView {
                chain: "c".into(),
                address: Some("a".into()),
                symbol: "ETH".into(),
                decimals: 18,
                balance: Some("1".into()),
                display: Some("0".into()),
                low_at: 0.01,
                spent_per_day: Some(0.1),
                days_left: Some(3.0),
                level: crate::wallets::Level::Low,
                problem: Some("p".into()),
                checked_at: Some(1),
                error: None,
                explorer: None,
            })
            .unwrap(),
        );
        check(
            "MessagePage",
            serde_json::to_value(MessagePage {
                total: 0,
                messages: vec![],
            })
            .unwrap(),
        );
        check(
            "SearchResult",
            serde_json::to_value(SearchView {
                query: "q".into(),
                matched: vec![],
                messages: vec![],
            })
            .unwrap(),
        );
        check(
            "Inbox",
            serde_json::to_value(InboxView {
                notifications: vec![],
            })
            .unwrap(),
        );
    }
}
