//! What the bridge UI reads.
//!
//! Two endpoints: where each route's trusted state stands right now, and — given a message
//! id — which batch carried it and what the enclave signed for that batch. Everything served
//! here is public: a quote, a set of message ids and a state root already on chain.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::chains::read_ism_state;
use crate::config::{ChainConfig, RouteConfig};
use tee_attestation::decode_ism_state;

/// One attested batch, as the prover recorded it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationRecord {
    pub height: u64,
    pub state_root: String,
    pub quote: String,
    pub measurements: Measurements,
    /// Every message id authorised together with this one.
    pub batch: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurements {
    #[serde(rename = "mrTd")]
    pub mr_td: String,
    #[serde(rename = "osImageHash")]
    pub os_image_hash: String,
    #[serde(rename = "composeHash")]
    pub compose_hash: String,
}

/// Where one route's trusted state stands, read live from the destination chain.
#[derive(Debug, Clone, Serialize)]
pub struct RouteStatus {
    pub name: String,
    pub origin: u32,
    pub destination: u32,
    pub ism: String,
    /// Origin height the ISM currently trusts, and the origin head time it was taken at.
    pub height: Option<u64>,
    pub timestamp: Option<u64>,
    #[serde(rename = "stateRoot")]
    pub state_root: Option<String>,
    /// Message ids in the most recent batches this route proved, newest first.
    pub batches: Vec<Batch>,
    /// The batch currently being proved, if any. Proving is minutes of CPU, so a route
    /// spends most of its time here rather than idle.
    pub proving: Option<Batch>,
    /// The origin's own head. Without it an idle route is indistinguishable from a stuck
    /// one - "trusted 571870, head 573707" says which.
    #[serde(rename = "originHead")]
    pub origin_head: Option<u64>,
    /// Set when the destination chain could not be reached this request.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Batch {
    pub height: u64,
    pub messages: Vec<String>,
}

/// Attestations are read from the proof store, so the API has no state of its own and a
/// restart loses nothing.
const RECENT_BATCHES: usize = 10;

#[derive(Clone)]
pub struct Api {
    root: Arc<PathBuf>,
    routes: Arc<Vec<RouteConfig>>,
}

impl Api {
    pub fn new(proof_dir: impl Into<PathBuf>, routes: Vec<RouteConfig>) -> Self {
        Self { root: Arc::new(proof_dir.into()), routes: Arc::new(routes) }
    }

    pub fn router(self) -> Router {
        Router::new()
            // Opening this port in a browser should show the routes, not raw JSON.
            .route("/", get(dashboard))
            .route("/api/status", get(status))
            .route("/api/attestation/{message_id}", get(attestation))
            .route("/api/health", get(|| async { "ok" }))
            .with_state(self)
    }

    /// The batch this route is proving right now, read from its staging file.
    fn in_flight(&self, route: &str) -> Option<Batch> {
        let staged = self.root.join(route).join("staging").join("attestation.json");
        let raw = std::fs::read(staged).ok()?;
        let record: serde_json::Value = serde_json::from_slice(&raw).ok()?;
        let state = hex::decode(record["attestation"]["new_state"].as_str()?).ok()?;
        let state = decode_ism_state(&state).ok()?;
        let messages = record["messages"].as_array()?;
        Some(Batch {
            height: state.height,
            messages: messages
                .iter()
                .filter_map(|m| m.as_str())
                .map(message_id)
                .collect(),
        })
    }

    /// The last few batches this route proved, newest first.
    fn recent_batches(&self, route: &str) -> Vec<Batch> {
        let mut batches: Vec<Batch> = read_dir(&self.root.join(route))
            .unwrap_or_default()
            .into_iter()
            .filter(|f| f.extension().is_some_and(|e| e == "json"))
            .filter_map(|f| serde_json::from_slice::<AttestationRecord>(&std::fs::read(f).ok()?).ok())
            .map(|record| Batch { height: record.height, messages: record.batch })
            .collect();
        batches.sort_by(|a, b| b.height.cmp(&a.height));
        batches.truncate(RECENT_BATCHES);
        batches
    }

    /// Scan recorded batches for one containing this message.
    fn find(&self, message_id: &str) -> Result<Option<AttestationRecord>> {
        let wanted = message_id.trim_start_matches("0x").to_lowercase();
        for route in read_dir(self.root.as_path())? {
            for file in read_dir(&route)? {
                if file.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                let record: AttestationRecord =
                    match serde_json::from_slice(&std::fs::read(&file)?) {
                        Ok(record) => record,
                        // A file the prover is still writing is not an error.
                        Err(_) => continue,
                    };
                if record
                    .batch
                    .iter()
                    .any(|id| id.trim_start_matches("0x").eq_ignore_ascii_case(&wanted))
                {
                    return Ok(Some(record));
                }
            }
        }
        Ok(None)
    }
}

/// A Hyperlane message id is the keccak of its encoding.
fn message_id(message_hex: &str) -> String {
    let raw = hex::decode(message_hex.trim_start_matches("0x")).unwrap_or_default();
    format!("0x{}", hex::encode(alloy_primitives::keccak256(raw)))
}

fn read_dir(path: &std::path::Path) -> Result<Vec<PathBuf>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(std::fs::read_dir(path)?.filter_map(|e| e.ok().map(|e| e.path())).collect())
}

/// The page this port serves. Small enough to embed, so the API ships as one binary.
async fn dashboard() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../ui/relayer.html"))
}

/// Read every route's trusted state.
///
/// Each route costs two chain reads and each shells out to `cast` or `celestia-appd`, so done
/// in sequence a five-route page takes half a minute. They are independent, so they run at
/// once and the page waits for the slowest rather than the sum.
async fn status(State(api): State<Api>) -> Json<Vec<RouteStatus>> {
    let mut reads = Vec::new();
    for (index, route) in api.routes.iter().enumerate() {
        let route = route.clone();
        let api = api.clone();
        reads.push(tokio::task::spawn_blocking(move || (index, read_route(&api, &route))));
    }

    let mut statuses: Vec<(usize, RouteStatus)> = Vec::new();
    for read in reads {
        if let Ok(result) = read.await {
            statuses.push(result);
        }
    }
    // Keep configuration order, which is the order a reader expects.
    statuses.sort_by_key(|(index, _)| *index);
    Json(statuses.into_iter().map(|(_, status)| status).collect())
}

fn read_route(api: &Api, route: &RouteConfig) -> RouteStatus {
    let mut status = RouteStatus {
        name: route.name.clone(),
        origin: route.origin.domain(),
        destination: route.destination.domain(),
        ism: route.ism_id.clone(),
        height: None,
        timestamp: None,
        state_root: None,
        batches: api.recent_batches(&route.name),
        proving: api.in_flight(&route.name),
        origin_head: read_origin_head(&route.origin),
        error: None,
    };

    match read_trusted_state(route) {
        Ok((root, height, timestamp)) => {
            status.state_root = Some(root);
            status.height = Some(height);
            status.timestamp = Some(timestamp);
        }
        Err(error) => status.error = Some(error.to_string()),
    }
    status
}

/// The newest L2 block Ethereum has confirmed, which is what an L2 route can attest up to.
fn read_confirmed_l2_block(rollup: &str, anchor: &str, l1_rpc: &str) -> Option<u64> {
    let (signature, line) = match rollup {
        // Base's registry hands back the root and the block it belongs to.
        "base" => ("getAnchorRoot()(bytes32,uint256)", 1),
        // Arbitrum's rollup has no equivalent view; its confirmed block only falls out of the
        // assertion preimage, which is the attestation's job rather than the dashboard's.
        _ => return None,
    };
    let output = std::process::Command::new("cast")
        .args(["call", anchor, signature, "--rpc-url", l1_rpc])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    text.lines().nth(line)?.split_whitespace().next()?.parse().ok()
}

fn read_origin_head(origin: &ChainConfig) -> Option<u64> {
    match origin {
        ChainConfig::Celestia { rpc, .. } => {
            let output = std::process::Command::new("celestia-appd")
                .args(["status", "--node", rpc, "-o", "json"])
                .output()
                .ok()?;
            let status: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
            status["sync_info"]["latest_block_height"].as_str()?.parse().ok()
        }
        // For an Ethereum origin the relevant head is the *finalized* one, because that is
        // all the enclave will attest.
        ChainConfig::Ethereum { execution_rpc, .. } => {
            let output = std::process::Command::new("cast")
                .args(["block", "finalized", "--field", "number", "--rpc-url", execution_rpc])
                .output()
                .ok()?;
            String::from_utf8(output.stdout).ok()?.trim().parse().ok()
        }
        // An L2's own head is not the useful number: the enclave attests the block Ethereum
        // has *confirmed*, which trails it by a challenge window. Reporting the head would
        // make every L2 route look permanently thousands of blocks behind.
        ChainConfig::EthereumL2 { rollup, l1_anchor_contract, l1, .. } => {
            let ChainConfig::Ethereum { execution_rpc, .. } = &**l1 else {
                return None;
            };
            read_confirmed_l2_block(rollup, l1_anchor_contract, execution_rpc)
        }
    }
}

fn read_trusted_state(route: &RouteConfig) -> Result<(String, u64, u64)> {
    let hex_state = read_ism_state(&route.destination, &route.ism_id)?;
    let raw = hex::decode(hex_state.trim_start_matches("0x"))?;
    let state = decode_ism_state(&raw)?;
    Ok((
        format!("0x{}", hex::encode(state.state_root)),
        state.height,
        state.timestamp,
    ))
}

async fn attestation(
    State(api): State<Api>,
    Path(message_id): Path<String>,
) -> Result<Json<AttestationRecord>, StatusCode> {
    match api.find(&message_id) {
        Ok(Some(record)) => Ok(Json(record)),
        // Not yet attested is the normal case for a fresh message, not a failure.
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn serve(api: Api, addr: &str) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "attestation api listening");
    axum::serve(listener, api.router()).await?;
    Ok(())
}
