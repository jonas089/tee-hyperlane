//! The dashboard API, served from the same process as the routes.
//!
//! Everything it reports comes from two places: each route's directory, where the route loop
//! writes its staged and finished batches and its markers, and each destination's ISM, read
//! live. It holds no state of its own.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::config::{self, Config};
use crate::destination::Destination;

/// How many finished batches the dashboard lists per route.
const RECENT_BATCHES: usize = 10;

#[derive(Clone)]
pub struct Api {
    proof_dir: Arc<PathBuf>,
    routes: Arc<Vec<RouteView>>,
    faucet: Option<Arc<Faucet>>,
}

struct RouteView {
    config: config::Route,
    origin_domain: u32,
    destination_domain: u32,
    destination: Box<dyn Destination>,
}

/// What the enclave measured, as shown next to each batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurements {
    #[serde(rename = "mrTd")]
    pub mr_td: String,
    #[serde(rename = "osImageHash")]
    pub os_image_hash: String,
    #[serde(rename = "composeHash")]
    pub compose_hash: String,
}

/// A finished batch, as `route.rs` files it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub height: u64,
    pub state_root: String,
    pub quote: String,
    pub measurements: Measurements,
    pub batch: Vec<String>,
}

impl Api {
    pub fn new(config: &Config) -> Result<Self> {
        let routes = config
            .routes
            .iter()
            .map(|r| {
                Ok(RouteView {
                    origin_domain: config.domain(&r.from)?,
                    destination_domain: config.domain(&r.to)?,
                    destination: config.destination(&r.to, &r.ism)?,
                    config: r.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            proof_dir: Arc::new(config.proof_dir()),
            routes: Arc::new(routes),
            faucet: Faucet::new(config)?.map(Arc::new),
        })
    }

    fn route_dir(&self, route: &str) -> PathBuf {
        self.proof_dir.join(route)
    }

    fn read_json(&self, route: &str, file: &str) -> Option<Value> {
        serde_json::from_slice(&std::fs::read(self.route_dir(route).join(file)).ok()?).ok()
    }

    fn records(&self, route: &str) -> Vec<Record> {
        let Ok(entries) = std::fs::read_dir(self.route_dir(route)) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok()?.path().to_str().map(PathBuf::from))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| serde_json::from_slice(&std::fs::read(p).ok()?).ok())
            .collect()
    }

    async fn status(&self, view: &RouteView) -> Value {
        let name = &view.config.name;
        let mut batches = self.records(name);
        batches.sort_by(|a, b| b.height.cmp(&a.height));
        batches.truncate(RECENT_BATCHES);
        // A batch attested but not yet delivered, from its staged record.
        let submitting = self
            .read_json(name, "staging/batch.json")
            .and_then(|staged| {
                let state = hex::decode(staged["attestation"]["new_state"].as_str()?).ok()?;
                let height = tee_node::state::IsmState::decode(&state).ok()?.height;
                let ids: Vec<String> = staged["messages"]
                    .as_array()?
                    .iter()
                    .filter_map(|m| {
                        Some(format!(
                            "0x{}",
                            hex::encode(alloy_primitives::keccak256(
                                hex::decode(m.as_str()?).ok()?
                            ))
                        ))
                    })
                    .collect();
                Some(json!({ "height": height, "messages": ids, "stage": "submitting" }))
            });
        let mut status = json!({
            "name": name,
            "origin": view.origin_domain,
            "destination": view.destination_domain,
            "ism": view.config.ism,
            "batches": batches.iter().map(|b| json!({ "height": b.height, "messages": b.batch })).collect::<Vec<_>>(),
            "proving": submitting,
            "originHead": self.read_json(name, "head.json").and_then(|h| h["target"].as_u64()),
            "blocked": self.read_json(name, "blocked.json"),
        });
        match view.destination.state().await {
            Ok(state) => {
                status["height"] = state.height.into();
                status["timestamp"] = state.timestamp.into();
                status["stateRoot"] = format!("0x{}", hex::encode(state.state_root)).into();
            }
            Err(e) => status["error"] = e.to_string().into(),
        }
        status
    }
}

pub async fn serve(api: Api, listen: &str) -> Result<()> {
    let app = Router::new()
        .route(
            "/",
            get(|| async { axum::response::Html(include_str!("../ui/relayer.html")) }),
        )
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/status", get(status))
        .route("/api/attestation/{message_id}", get(attestation))
        .route("/api/faucet", get(faucet_info).post(faucet_claim))
        .route("/api/faucet/{address}", get(faucet_claimed))
        .with_state(api);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!(listen, "api listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn status(State(api): State<Api>) -> Json<Vec<Value>> {
    let mut out = Vec::new();
    for view in api.routes.iter() {
        out.push(api.status(view).await);
    }
    Json(out)
}

/// The batch that attested a message, so the UI can show what vouched for a transfer.
async fn attestation(
    State(api): State<Api>,
    Path(message_id): Path<String>,
) -> Result<Json<Record>, StatusCode> {
    let wanted = message_id.trim_start_matches("0x").to_lowercase();
    api.routes
        .iter()
        .flat_map(|v| api.records(&v.config.name))
        .find(|r| {
            r.batch
                .iter()
                .any(|id| id.trim_start_matches("0x").eq_ignore_ascii_case(&wanted))
        })
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// The measurements a quote and its event log carry, for display next to a batch.
pub fn measurements(quote: &str, event_log: &str) -> Result<Measurements> {
    let id = crate::identity::Identity::from_quote(quote, event_log)?;
    Ok(Measurements {
        mr_td: id.mr_td,
        os_image_hash: id.os_image_hash,
        compose_hash: id.compose_hash,
    })
}

// ---------------------------------------------------------------- faucet
//
// One grant of test TIA per Celestia address, from the chain named by `[faucet]` in the config.
// A claim is recorded as a file before anything is sent, so two requests racing for one address
// cannot both be paid.

const GRANT_UTIA: u64 = 1_000_000_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaucetConfig {
    /// The Celestia chain, by name, to pay out on.
    pub chain: String,
    /// The funded key in that chain's keyring.
    #[serde(default = "default_faucet_key")]
    pub key: String,
}

fn default_faucet_key() -> String {
    "faucet".into()
}

struct Faucet {
    claims: PathBuf,
    rpc: String,
    chain_id: String,
    home: String,
    key: String,
}

impl Faucet {
    fn new(config: &Config) -> Result<Option<Self>> {
        let Some(faucet) = &config.faucet else {
            return Ok(None);
        };
        let chain: crate::celestia::Config = config.chain(&faucet.chain)?;
        Ok(Some(Self {
            claims: config.proof_dir().join(".faucet"),
            rpc: chain.rpc,
            chain_id: chain.chain_id,
            home: chain
                .home
                .or_else(|| std::env::var("CELHOME").ok())
                .context("the faucet chain needs `home` or CELHOME")?,
            key: faucet.key.clone(),
        }))
    }

    async fn claim(&self, address: &str) -> Result<String, (StatusCode, String)> {
        let valid = address.len() == 47
            && address.starts_with("celestia1")
            && address[9..]
                .chars()
                .all(|c| "qpzry9x8gf2tvdw0s3jn54khce6mua7l".contains(c));
        if !valid {
            return Err((StatusCode::BAD_REQUEST, "not a Celestia address".into()));
        }
        let fail = |e: String| {
            (
                StatusCode::BAD_GATEWAY,
                format!("the faucet could not send: {e}"),
            )
        };
        std::fs::create_dir_all(&self.claims).map_err(|e| fail(e.to_string()))?;
        let marker = self.claims.join(address);
        let mut file = match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&marker)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err((
                    StatusCode::CONFLICT,
                    "this address has already claimed".into(),
                ))
            }
            Err(e) => return Err(fail(e.to_string())),
        };
        let appd = std::env::var("APPD").unwrap_or_else(|_| "celestia-appd".into());
        let amount = format!("{GRANT_UTIA}utia");
        let sent = tokio::process::Command::new(&appd)
            .args([
                "tx",
                "bank",
                "send",
                &self.key,
                address,
                &amount,
                "--home",
                &self.home,
                "--keyring-backend",
                "test",
            ])
            .args([
                "--chain-id",
                &self.chain_id,
                "--node",
                &self.rpc,
                "--fees",
                "200000utia",
                "--gas",
                "200000",
                "-y",
                "-o",
                "json",
            ])
            .output()
            .await
            .map_err(|e| fail(e.to_string()))
            .and_then(|out| {
                let body: Value = serde_json::from_slice(&out.stdout)
                    .map_err(|_| fail(String::from_utf8_lossy(&out.stderr).trim().to_string()))?;
                match body["code"].as_u64() {
                    Some(0) | None => body["txhash"]
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| fail("no txhash".into())),
                    Some(code) => Err(fail(format!("code {code}: {}", body["raw_log"]))),
                }
            });
        match &sent {
            Ok(tx) => {
                let _ = file.write_all(
                    json!({ "address": address, "tx_hash": tx })
                        .to_string()
                        .as_bytes(),
                );
                info!(address, tx, "faucet paid");
            }
            Err((_, e)) => {
                drop(file);
                let _ = std::fs::remove_file(&marker);
                warn!(address, error = %e, "faucet send failed");
            }
        }
        sent
    }
}

async fn faucet_info(State(api): State<Api>) -> Json<Value> {
    Json(json!({ "enabled": api.faucet.is_some(), "amountTia": GRANT_UTIA / 1_000_000 }))
}

async fn faucet_claimed(State(api): State<Api>, Path(address): Path<String>) -> Json<Value> {
    let claimed = api
        .faucet
        .as_ref()
        .is_some_and(|f| f.claims.join(address.trim()).exists());
    Json(json!({ "address": address, "claimed": claimed }))
}

async fn faucet_claim(
    State(api): State<Api>,
    Json(req): Json<BTreeMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let deny = |(code, why): (StatusCode, String)| (code, Json(json!({ "error": why })));
    let faucet = api.faucet.as_ref().ok_or_else(|| {
        deny((
            StatusCode::SERVICE_UNAVAILABLE,
            "the faucet is not configured".into(),
        ))
    })?;
    let address = req
        .get("address")
        .map(|a| a.trim().to_string())
        .unwrap_or_default();
    let tx = faucet.claim(&address).await.map_err(deny)?;
    Ok(Json(
        json!({ "address": address, "amount_tia": GRANT_UTIA / 1_000_000, "tx_hash": tx }),
    ))
}
