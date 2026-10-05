//! Tracks each transfer on our routes from dispatch to delivery.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy_primitives::{keccak256, Address, U256};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, info, warn};

use crate::config::{self, Config};
use crate::destination::Destination;
use crate::origin::evm;
use crate::origin::l1::celestia;
use crate::route::{now, Delivered, Parked};

/// The most origin blocks one poll reads, so catching up after downtime is spread out.
const MAX_SPAN_EVM: u64 = 100_000;
const MAX_SPAN_CELESTIA: u64 = 50_000;

/// A route's own chain names and domains, its ISM, and how long it normally takes.
pub struct TrackedRoute {
    pub name: String,
    pub from: String,
    pub to: String,
    pub origin_domain: u32,
    pub destination_domain: u32,
    pub ism: String,
    pub routers: Vec<[u8; 32]>,
    pub expected_latency: u64,
    /// The route loop's directory: its markers, staged batch, parked and delivered messages.
    pub dir: PathBuf,
    pub destination: Box<dyn Destination>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainInfo {
    pub name: String,
    pub kind: String,
    pub domain: u32,
    pub explorer: Option<String>,
}

impl ChainInfo {
    pub fn tx_url(&self, tx: &str) -> Option<String> {
        self.explorer
            .as_ref()
            .map(|e| format!("{}/tx/{tx}", e.trim_end_matches('/')))
    }
}

/// How long a transfer on a route from `kind` normally takes to be delivered, generously: past
/// this it is overdue, whatever else is known.
pub fn expected_latency(kind: &str) -> u64 {
    match kind {
        "celestia" => 15 * 60,
        // Eden's signed headers reach Celestia every minute or two.
        "eden" => 15 * 60,
        // Finality is two epochs, about 13 minutes, then one attestation.
        "ethereum" => 60 * 60,
        // Every block is signed by the sequencer as it is made, so one attestation.
        "arbitrum" | "base" => 15 * 60,
        _ => 60 * 60,
    }
}

/// What the tracker waits on for a transfer from `kind`, in words.
pub fn waiting_on(kind: &str) -> &'static str {
    match kind {
        "celestia" => "the next attestation of Celestia",
        "eden" => "Eden's signed header reaching Celestia",
        "ethereum" => "Sepolia finality, about 13 minutes",
        "arbitrum" => "the next block Arbitrum's sequencer signs on its feed",
        "base" => "the next block Base's sequencer signs on p2p",
        _ => "the origin to finalize",
    }
}

// ---------------------------------------------------------------- the records

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MessageRecord {
    /// The message id, 0x-prefixed.
    pub id: String,
    pub route: String,
    pub nonce: u32,
    /// The origin router, as a 32-byte Hyperlane address.
    pub sender: String,
    /// The destination router, as a 32-byte Hyperlane address.
    pub recipient: String,
    pub transfer: Option<Transfer>,
    /// Absent only for transfers recorded from batch files that predate the tracker.
    pub dispatch: Option<Dispatch>,
    pub first_seen_at: u64,
    /// When the route loop first reported an origin head at or past the dispatch block: the
    /// moment the transfer became attestable.
    pub attestable_at: Option<u64>,
    pub verified: Option<Verified>,
    pub delivery: Option<DeliveryInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    /// The receiving account, as the destination writes it.
    pub recipient: String,
    /// The same account as a 32-byte Hyperlane address.
    pub recipient_hex: String,
    /// In the token's base units, decimal.
    pub amount: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Dispatch {
    pub tx: String,
    pub block: u64,
    pub timestamp: u64,
    /// The account that sent the dispatching transaction.
    pub from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Verified {
    pub at: u64,
    pub ism_height: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryInfo {
    pub at: u64,
    /// Our transaction; absent when someone else delivered it, or before the tracker existed.
    pub tx: Option<String>,
}

/// The ISM of a route, as last read.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IsmReading {
    pub height: Option<u64>,
    pub timestamp: Option<u64>,
    pub read_at: Option<u64>,
    pub failing_since: Option<u64>,
    pub error: Option<String>,
}

/// How the tracker's watch on one origin is going.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Watch {
    /// The last origin block scanned.
    pub cursor: Option<u64>,
    pub tip: Option<u64>,
    pub last_ok_at: Option<u64>,
    pub failing_since: Option<u64>,
    pub error: Option<String>,
}

/// Everything the tracker knows, guarded by one lock that is never held across a read.
#[derive(Default)]
pub struct State {
    pub messages: BTreeMap<String, MessageRecord>,
    pub isms: BTreeMap<String, IsmReading>,
    pub watches: BTreeMap<String, Watch>,
    pub inbox: Vec<crate::monitor::Notification>,
    pub wallets: BTreeMap<String, crate::wallets::WalletReading>,
    pub last_sweep_at: Option<u64>,
}

// ---------------------------------------------------------------- reading an origin

enum Scanner {
    Evm { rpc: evm::Rpc, mailbox: Address },
    Celestia { rpc: celestia::Rpc },
}

struct Watcher {
    chain: String,
    scanner: Scanner,
    /// Blocks behind the tip to stay, so a reorg rarely takes back a recorded dispatch.
    confirmations: u64,
}

/// One dispatch to one of our routers, as the origin reported it.
struct Seen {
    tx: String,
    block: u64,
    from: Option<String>,
    message: Vec<u8>,
}

impl Watcher {
    fn new(config: &Config, chain: &str) -> Result<Self> {
        let kind = config.kind(chain)?.to_string();
        let table = config
            .chains
            .get(chain)
            .with_context(|| format!("no chain `{chain}`"))?;
        let text = |k: &str| table.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let scanner = match kind.as_str() {
            "celestia" => Scanner::Celestia {
                rpc: celestia::Rpc::new(&text("rpc").context("rpc")?)?,
            },
            _ => {
                let rpc = text("rpc").context("rpc")?;
                Scanner::Evm {
                    rpc: evm::Rpc::new(&rpc, text("logs_rpc").as_deref()),
                    mailbox: text("mailbox").context("mailbox")?.parse()?,
                }
            }
        };
        Ok(Self {
            chain: chain.to_string(),
            confirmations: if kind == "ethereum" { 2 } else { 0 },
            scanner,
        })
    }

    async fn tip(&self) -> Result<u64> {
        let tip = match &self.scanner {
            Scanner::Evm { rpc, .. } => {
                evm::quantity(&rpc.read("eth_blockNumber", json!([])).await?)?
            }
            Scanner::Celestia { rpc } => rpc.latest_height().await?,
        };
        Ok(tip.saturating_sub(self.confirmations))
    }

    fn span(&self) -> u64 {
        match self.scanner {
            Scanner::Evm { .. } => MAX_SPAN_EVM,
            Scanner::Celestia { .. } => MAX_SPAN_CELESTIA,
        }
    }

    /// Every dispatch over `from..=to` to one of `recipients`, with block times.
    async fn scan(&self, recipients: &[[u8; 32]], from: u64, to: u64) -> Result<Vec<(Seen, u64)>> {
        let mut times: BTreeMap<u64, u64> = BTreeMap::new();
        let mut out = Vec::new();
        match &self.scanner {
            Scanner::Evm { rpc, mailbox } => {
                for (log, message) in rpc.dispatches_to(*mailbox, recipients, from, to).await? {
                    let block = evm::quantity(&log["blockNumber"])?;
                    let tx = log["transactionHash"]
                        .as_str()
                        .context("transactionHash")?
                        .to_lowercase();
                    let time = match times.get(&block) {
                        Some(t) => *t,
                        None => {
                            let b = rpc
                                .read(
                                    "eth_getBlockByNumber",
                                    json!([evm::hex_number(block), false]),
                                )
                                .await?;
                            let t = evm::quantity(&b["timestamp"])?;
                            times.insert(block, t);
                            t
                        }
                    };
                    let from = rpc
                        .read("eth_getTransactionByHash", json!([tx]))
                        .await
                        .ok()
                        .and_then(|t| t["from"].as_str().map(str::to_lowercase));
                    out.push((
                        Seen {
                            tx,
                            block,
                            from,
                            message,
                        },
                        time,
                    ));
                }
            }
            Scanner::Celestia { rpc } => {
                for (hash, height, events) in rpc.dispatch_txs(from, to).await? {
                    let attribute = |ev: &tendermint::abci::Event, key: &str| {
                        ev.attributes.iter().find_map(|a| {
                            (a.key_str().ok()? == key).then(|| {
                                a.value_str().ok().map(|v| v.trim_matches('"').to_string())
                            })?
                        })
                    };
                    // The signer, as the SDK reports it on the transaction's message event.
                    let sender = events
                        .iter()
                        .filter(|e| e.kind == "message")
                        .find_map(|e| attribute(e, "sender"))
                        .filter(|s| s.starts_with("celestia1"));
                    let time = match times.get(&height) {
                        Some(t) => *t,
                        None => {
                            let t = rpc.block_time(height).await?;
                            times.insert(height, t);
                            t
                        }
                    };
                    for ev in events
                        .iter()
                        .filter(|e| e.kind == celestia::DISPATCH_EVENT_NAME)
                    {
                        let Some(m) = attribute(ev, "message") else {
                            continue;
                        };
                        let message = hex::decode(m.trim_start_matches("0x"))?;
                        let recipient = hyperlane_types::decode_hyperlane_message(&message)
                            .map(|d| d.recipient)
                            .ok();
                        if !recipient.is_some_and(|r| recipients.contains(&r)) {
                            continue;
                        }
                        out.push((
                            Seen {
                                tx: hash.clone(),
                                block: height,
                                from: sender.clone(),
                                message,
                            },
                            time,
                        ));
                    }
                }
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- the tracker

pub struct Tracker {
    dir: PathBuf,
    pub routes: Vec<TrackedRoute>,
    pub wallets: Vec<crate::wallets::WatchedWallet>,
    pub chains: BTreeMap<String, ChainInfo>,
    watchers: Vec<Watcher>,
    pub track_secs: u64,
    pub started_at: u64,
    pub state: Mutex<State>,
}

/// A 20- or 32-byte address as the 32-byte Hyperlane form.
pub fn to_32(hex_address: &str) -> Option<[u8; 32]> {
    let raw = hex::decode(hex_address.trim_start_matches("0x")).ok()?;
    if raw.is_empty() || raw.len() > 32 {
        return None;
    }
    let mut padded = [0u8; 32];
    padded[32 - raw.len()..].copy_from_slice(&raw);
    Some(padded)
}

/// A 32-byte account as a chain of `kind` writes it: bech32 on Celestia, 20-byte hex on EVM
/// where it fits.
pub fn show_account(kind: &str, account: &[u8; 32]) -> String {
    let short = account[..12].iter().all(|b| *b == 0);
    match (kind, short) {
        ("celestia", true) => crate::bech32::encode("celestia", &account[12..]),
        (_, true) => format!("0x{}", hex::encode(&account[12..])),
        _ => format!("0x{}", hex::encode(account)),
    }
}

impl Tracker {
    pub fn new(config: &Config) -> Result<Arc<Self>> {
        let dir = config.proof_dir().join("tracker");
        std::fs::create_dir_all(&dir)?;
        let mut chains = BTreeMap::new();
        for (name, table) in &config.chains {
            let kind = config.kind(name)?.to_string();
            let domain = config.domain(name)?;
            let explorer = table
                .get("explorer")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| default_explorer(domain).map(str::to_string));
            chains.insert(
                name.clone(),
                ChainInfo {
                    name: name.clone(),
                    kind,
                    domain,
                    explorer,
                },
            );
        }
        let routes = config
            .routes
            .iter()
            .map(|r| tracked_route(config, r))
            .collect::<Result<Vec<_>>>()?;
        let mut origins: Vec<&str> = config.routes.iter().map(|r| r.from.as_str()).collect();
        origins.sort();
        origins.dedup();
        let watchers = origins
            .into_iter()
            .map(|c| Watcher::new(config, c))
            .collect::<Result<Vec<_>>>()?;

        let mut state = State {
            messages: load(&dir.join("messages.json")),
            watches: load(&dir.join("watches.json")),
            inbox: load(&dir.join("inbox.json")),
            wallets: load(&dir.join("wallets.json")),
            ..Default::default()
        };
        for route in &routes {
            backfill(route, &chains, &mut state.messages);
        }
        let wallets = watched_wallets(config, &chains)?;
        let tracker = Arc::new(Self {
            dir,
            routes,
            wallets,
            chains,
            watchers,
            track_secs: config.track_secs.max(1),
            started_at: now(),
            state: Mutex::new(state),
        });
        tracker.save();
        Ok(tracker)
    }

    /// Every origin chain the tracker watches.
    pub fn origins(&self) -> impl Iterator<Item = &str> {
        self.watchers.iter().map(|w| w.chain.as_str())
    }

    pub fn chain(&self, name: &str) -> Option<&ChainInfo> {
        self.chains.get(name)
    }

    pub fn route(&self, name: &str) -> Option<&TrackedRoute> {
        self.routes.iter().find(|r| r.name == name)
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Writes run one at a time: the state lock is held until the last file is renamed.
    pub fn save(&self) {
        let state = self.lock();
        for (file, result) in [
            ("messages.json", serde_json::to_vec(&state.messages)),
            ("watches.json", serde_json::to_vec(&state.watches)),
            ("inbox.json", serde_json::to_vec(&state.inbox)),
            ("wallets.json", serde_json::to_vec(&state.wallets)),
        ] {
            let saved = result
                .map_err(anyhow::Error::from)
                .and_then(|bytes| write_atomic(&self.dir.join(file), &bytes));
            if let Err(e) = saved {
                warn!(file, error = %e, "could not save tracker state");
            }
        }
    }

    /// Every watcher and the monitor, forever.
    pub fn spawn(self: &Arc<Self>, tasks: &mut tokio::task::JoinSet<()>) {
        for index in 0..self.watchers.len() {
            let tracker = Arc::clone(self);
            tasks.spawn(async move {
                let mut streak = crate::Streak::default();
                let chain = tracker.watchers[index].chain.clone();
                loop {
                    match tracker.poll(index).await {
                        Ok(()) => {
                            if let Some((failures, lasted)) = streak.ok() {
                                info!(chain, failures, after = %crate::monitor::duration(lasted.as_secs()), "watching again");
                            }
                        }
                        Err(e) => {
                            let error = crate::brief(&format!("{e:#}"));
                            if let Some(n) = streak.fail(&error) {
                                warn!(chain, error, consecutive_failures = n, "cannot read the origin to watch for transfers");
                            }
                        }
                    }
                    tracker.save();
                    tokio::time::sleep(Duration::from_secs(tracker.track_secs)).await;
                }
            });
        }
        let tracker = Arc::clone(self);
        tasks.spawn(async move {
            let mut streaks: BTreeMap<String, crate::Streak> = BTreeMap::new();
            loop {
                let readings = crate::wallets::check(&tracker.wallets).await;
                let at = now();
                {
                    let mut state = tracker.lock();
                    for (chain, reading) in readings {
                        let streak = streaks.entry(chain.clone()).or_default();
                        let entry = state.wallets.entry(chain.clone()).or_default();
                        match reading {
                            Ok(w) => {
                                entry.record(w.address, w.balance, at);
                                streak.ok();
                            }
                            Err(e) => {
                                let error = crate::brief(&format!("{e:#}"));
                                if streak.fail(&error).is_some() {
                                    warn!(chain, error, "cannot read the relayer's gas wallet");
                                }
                                entry.failing_since.get_or_insert(at);
                                entry.error = Some(error);
                            }
                        }
                    }
                }
                tracker.save();
                tokio::time::sleep(Duration::from_secs(crate::wallets::CHECK_EVERY)).await;
            }
        });
        let tracker = Arc::clone(self);
        tasks.spawn(async move {
            // Slack runs in its own task, fed changes as they happen, so a slow or refusing
            // Slack can never hold up the sweep.
            let (to_slack, from_monitor) = tokio::sync::mpsc::unbounded_channel();
            if let Some(slack) = crate::slack::Slack::from_env() {
                let tracker = Arc::clone(&tracker);
                tokio::spawn(crate::slack::run(slack, tracker, from_monitor));
            }
            // The first status line waits one interval, so the watchers have read every origin.
            let mut last_summary = now();
            loop {
                for change in crate::monitor::sweep(&tracker) {
                    let _ = to_slack.send(change);
                }
                if now().saturating_sub(last_summary) >= crate::monitor::SUMMARY_EVERY {
                    crate::monitor::log_summary(&tracker);
                    last_summary = now();
                }
                tracker.save();
                tokio::time::sleep(Duration::from_secs(tracker.track_secs)).await;
            }
        });
    }

    async fn poll(&self, index: usize) -> Result<()> {
        let watcher = &self.watchers[index];
        let chain = watcher.chain.clone();
        let result = self.scan(watcher).await;
        {
            let mut state = self.lock();
            let watch = state.watches.entry(chain.clone()).or_default();
            match &result {
                Ok(()) => {
                    watch.last_ok_at = Some(now());
                    watch.failing_since = None;
                    watch.error = None;
                }
                Err(e) => {
                    watch.failing_since.get_or_insert(now());
                    watch.error = Some(crate::brief(&format!("{e:#}")));
                }
            }
        }
        for route in self.routes.iter().filter(|r| r.from == chain) {
            self.progress(route).await;
        }
        result
    }

    /// Read the origin's new blocks and record every dispatch to our routers.
    async fn scan(&self, watcher: &Watcher) -> Result<()> {
        let routes: Vec<&TrackedRoute> = self
            .routes
            .iter()
            .filter(|r| r.from == watcher.chain)
            .collect();
        let tip = watcher.tip().await.context("reading the origin's head")?;
        let cursor = self
            .lock()
            .watches
            .get(&watcher.chain)
            .and_then(|w| w.cursor);
        let cursor = match cursor {
            Some(c) => c,
            // First run: start where the least advanced ISM is. Everything before it is
            // already verified, and was delivered by the batches the backfill read.
            None => {
                let mut lowest = u64::MAX;
                for route in &routes {
                    let state =
                        route.destination.state().await.with_context(|| {
                            format!("reading {}'s ISM to start from", route.name)
                        })?;
                    lowest = lowest.min(state.height);
                }
                info!(chain = watcher.chain, from = lowest, "tracker starting");
                lowest
            }
        };
        {
            let mut state = self.lock();
            let watch = state.watches.entry(watcher.chain.clone()).or_default();
            watch.tip = Some(tip);
            watch.cursor = Some(cursor);
        }
        if tip <= cursor {
            return Ok(());
        }
        let to = tip.min(cursor + watcher.span());
        let recipients: Vec<[u8; 32]> = routes.iter().flat_map(|r| r.routers.clone()).collect();
        let found = watcher.scan(&recipients, cursor + 1, to).await?;

        let mut state = self.lock();
        for (seen, time) in found {
            let Ok(decoded) = hyperlane_types::decode_hyperlane_message(&seen.message) else {
                continue;
            };
            let Some(route) = routes.iter().find(|r| {
                r.destination_domain == decoded.destination
                    && r.routers.contains(&decoded.recipient)
            }) else {
                continue;
            };
            let id = format!("0x{}", hex::encode(keccak256(&seen.message)));
            if state.messages.contains_key(&id) {
                continue;
            }
            let mut record = new_record(route, &self.chains, &id, &seen.message);
            record.dispatch = Some(Dispatch {
                tx: seen.tx,
                block: seen.block,
                timestamp: time,
                from: seen.from,
            });
            info!(
                route = route.name,
                id,
                amount = record.transfer.as_ref().map_or("", |t| t.amount.as_str()),
                to = record
                    .transfer
                    .as_ref()
                    .map_or("", |t| t.recipient.as_str()),
                "new transfer"
            );
            state.messages.insert(id, record);
        }
        state
            .watches
            .entry(watcher.chain.clone())
            .or_default()
            .cursor = Some(to);
        Ok(())
    }

    /// Where every open transfer on `route` has got to.
    async fn progress(&self, route: &TrackedRoute) {
        let reading = route.destination.state().await;
        let ism_height = {
            let mut state = self.lock();
            let ism = state.isms.entry(route.name.clone()).or_default();
            match &reading {
                Ok(s) => {
                    *ism = IsmReading {
                        height: Some(s.height),
                        timestamp: Some(s.timestamp),
                        read_at: Some(now()),
                        failing_since: None,
                        error: None,
                    };
                    Some(s.height)
                }
                Err(e) => {
                    ism.failing_since.get_or_insert(now());
                    ism.error = Some(crate::brief(&format!("{e:#}")));
                    None
                }
            }
        };
        let head = route_head(&route.dir).map(|h| h.0);

        let open: Vec<MessageRecord> = self
            .lock()
            .messages
            .values()
            .filter(|m| m.route == route.name && m.delivery.is_none())
            .cloned()
            .collect();
        for mut record in open {
            let before = record.clone();
            let block = record.dispatch.as_ref().map(|d| d.block);
            if let (Some(block), Some(head)) = (block, head) {
                if head >= block && record.attestable_at.is_none() {
                    record.attestable_at = Some(now());
                }
            }
            if let (Some(block), Some(height)) = (block, ism_height) {
                if height >= block && record.verified.is_none() {
                    record.verified = Some(Verified {
                        at: now(),
                        ism_height: height,
                    });
                }
            }
            let id = record.id.trim_start_matches("0x").to_string();
            if let Some(d) =
                read_json::<Delivered>(&route.dir.join("delivered").join(format!("{id}.json")))
            {
                record.delivery = Some(DeliveryInfo { at: d.at, tx: d.tx });
            } else if record.verified.is_some() {
                // Someone else may have delivered it; the mailbox is the truth either way.
                if let Ok(true) = route.destination.delivered(&id).await {
                    record.delivery = Some(DeliveryInfo {
                        at: now(),
                        tx: None,
                    });
                }
            }
            if record.verified.is_some() && before.verified.is_none() {
                debug!(route = route.name, id = record.id, "transfer verified");
            }
            if let (Some(d), None) = (&record.delivery, &before.delivery) {
                let sent = record
                    .dispatch
                    .as_ref()
                    .map_or(record.first_seen_at, |d| d.timestamp);
                info!(
                    route = route.name,
                    id = record.id,
                    after = %crate::monitor::duration(d.at.saturating_sub(sent)),
                    "transfer delivered"
                );
            }
            if record != before {
                self.lock().messages.insert(record.id.clone(), record);
            }
        }
    }

    /// The parked message for `id` on `route`, if its delivery is failing.
    pub fn parked(&self, route: &TrackedRoute, id: &str) -> Option<Parked> {
        read_json(
            &route
                .dir
                .join("undelivered")
                .join(format!("{}.json", id.trim_start_matches("0x"))),
        )
    }

    pub fn parked_all(&self, route: &TrackedRoute) -> Vec<Parked> {
        let Ok(entries) = std::fs::read_dir(route.dir.join("undelivered")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| read_json(&e.path()))
            .collect()
    }
}

fn tracked_route(config: &Config, r: &config::Route) -> Result<TrackedRoute> {
    Ok(TrackedRoute {
        name: r.name.clone(),
        from: r.from.clone(),
        to: r.to.clone(),
        origin_domain: config.domain(&r.from)?,
        destination_domain: config.domain(&r.to)?,
        ism: r.ism.clone(),
        routers: r.routers.iter().filter_map(|a| to_32(a)).collect(),
        expected_latency: r
            .expected_latency_secs
            .unwrap_or_else(|| expected_latency(config.kind(&r.from).unwrap_or(""))),
        dir: config.proof_dir().join(&r.name),
        destination: config.destination(&r.to, &r.ism)?,
    })
}

/// One gas wallet per chain some route delivers to.
fn watched_wallets(
    config: &Config,
    chains: &BTreeMap<String, ChainInfo>,
) -> Result<Vec<crate::wallets::WatchedWallet>> {
    let mut out: Vec<crate::wallets::WatchedWallet> = Vec::new();
    for r in &config.routes {
        if out.iter().any(|w| w.chain == r.to) {
            continue;
        }
        let kind = chains.get(&r.to).map_or("", |c| c.kind.as_str());
        let (symbol, decimals, low) = crate::wallets::defaults(kind);
        let table = config.chains.get(&r.to);
        let field = |k: &str| table.and_then(|t| t.get(k));
        out.push(crate::wallets::WatchedWallet {
            chain: r.to.clone(),
            symbol: field("gas_symbol")
                .and_then(|v| v.as_str())
                .unwrap_or(symbol)
                .to_string(),
            decimals: field("gas_decimals")
                .and_then(|v| v.as_integer())
                .map_or(decimals, |d| d as u32),
            low: field("low_balance")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .unwrap_or(low),
            destination: config.destination(&r.to, &r.ism)?,
        });
    }
    Ok(out)
}

fn default_explorer(domain: u32) -> Option<&'static str> {
    match domain {
        11155111 => Some("https://sepolia.etherscan.io"),
        421614 => Some("https://sepolia.arbiscan.io"),
        84532 => Some("https://sepolia.basescan.org"),
        _ => None,
    }
}

fn new_record(
    route: &TrackedRoute,
    chains: &BTreeMap<String, ChainInfo>,
    id: &str,
    message: &[u8],
) -> MessageRecord {
    let decoded = hyperlane_types::decode_hyperlane_message(message).ok();
    let kind = chains.get(&route.to).map_or("", |c| c.kind.as_str());
    let transfer = decoded
        .as_ref()
        .and_then(|d| hyperlane_types::decode_token_message_body(&d.body).ok())
        .map(|t| Transfer {
            recipient: show_account(kind, &t.recipient),
            recipient_hex: format!("0x{}", hex::encode(t.recipient)),
            amount: U256::from_be_bytes(t.amount).to_string(),
        });
    MessageRecord {
        id: id.to_string(),
        route: route.name.clone(),
        nonce: decoded.as_ref().map_or(0, |d| d.nonce),
        sender: decoded
            .as_ref()
            .map_or_else(String::new, |d| format!("0x{}", hex::encode(d.sender))),
        recipient: decoded
            .as_ref()
            .map_or_else(String::new, |d| format!("0x{}", hex::encode(d.recipient))),
        transfer,
        dispatch: None,
        first_seen_at: now(),
        attestable_at: None,
        verified: None,
        delivery: None,
    }
}

/// Transfers the route delivered before the tracker existed, from its finished batch files.
/// A batch file is written only once every message in it is delivered, so they are delivered.
fn backfill(
    route: &TrackedRoute,
    chains: &BTreeMap<String, ChainInfo>,
    messages: &mut BTreeMap<String, MessageRecord>,
) {
    let Ok(entries) = std::fs::read_dir(&route.dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(height) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let Some(batch) = read_json::<serde_json::Value>(&path) else {
            continue;
        };
        let at = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        for m in batch["messages"].as_array().into_iter().flatten() {
            let Some(bytes) = m.as_str().and_then(|m| hex::decode(m).ok()) else {
                continue;
            };
            let Ok(decoded) = hyperlane_types::decode_hyperlane_message(&bytes) else {
                continue;
            };
            if decoded.destination != route.destination_domain
                || !route.routers.contains(&decoded.recipient)
            {
                continue;
            }
            let id = format!("0x{}", hex::encode(keccak256(&bytes)));
            messages.entry(id.clone()).or_insert_with(|| {
                let mut record = new_record(route, chains, &id, &bytes);
                record.first_seen_at = at;
                record.verified = Some(Verified {
                    at,
                    ism_height: height,
                });
                record.delivery = Some(DeliveryInfo { at, tx: None });
                record
            });
        }
    }
}

/// The route loop's last reported origin head, and when it wrote it.
pub fn route_head(dir: &std::path::Path) -> Option<(u64, u64)> {
    let head: serde_json::Value = read_json(&dir.join("head.json"))?;
    Some((head["target"].as_u64()?, head["updated_at"].as_u64()?))
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Flushed before the rename, so a crash leaves the old file or the new one, never half of one.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// A saved state file, or empty when there is none. One that exists but does not parse is
/// moved aside and reported rather than quietly replaced: the tracker then rebuilds from the
/// chains, which costs a rescan, and the old file is still there to look at.
fn load<T: serde::de::DeserializeOwned + Default>(path: &std::path::Path) -> T {
    let Ok(bytes) = std::fs::read(path) else {
        return T::default();
    };
    match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(e) => {
            let aside = path.with_extension(format!("unreadable-{}", now()));
            let _ = std::fs::rename(path, &aside);
            tracing::error!(
                file = %path.display(),
                moved_to = %aside.display(),
                error = %e,
                "tracker state unreadable; starting it afresh"
            );
            T::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_are_shown_the_way_their_chain_writes_them() {
        let mut account = [0u8; 32];
        account[12..].copy_from_slice(&[0xab; 20]);
        assert_eq!(
            show_account("ethereum", &account),
            format!("0x{}", "ab".repeat(20))
        );
        assert!(show_account("celestia", &account).starts_with("celestia1"));
        let wide = [0x11u8; 32];
        assert_eq!(
            show_account("celestia", &wide),
            format!("0x{}", "11".repeat(32))
        );
    }

    #[test]
    fn an_unreadable_state_file_is_set_aside_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.json");
        assert!(load::<Vec<u8>>(&path).is_empty(), "absent is empty");
        write_atomic(&path, b"[1,2]").unwrap();
        assert_eq!(load::<Vec<u8>>(&path), vec![1, 2]);
        std::fs::write(&path, b"[1,").unwrap();
        assert!(load::<Vec<u8>>(&path).is_empty());
        assert!(!path.exists());
        let aside: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(aside.len(), 1, "the broken file is kept");
    }

    #[test]
    fn a_twenty_byte_router_is_padded() {
        let padded = to_32("0x9822eE81C82138F88D759faef1AC168aDfEe1467").unwrap();
        assert_eq!(&padded[..12], &[0u8; 12]);
        assert_eq!(
            hex::encode(&padded[12..]),
            "9822ee81c82138f88d759faef1ac168adfee1467"
        );
    }
}
