//! A gas oracle for the Celestia IGP.
//!
//! Hyperlane's fee quote is only as good as the numbers behind it, and those numbers are not
//! self-updating: nothing on chain knows what TIA or ETH is worth. This service reads both,
//! pushes the result to the IGP once an hour, and serves a page showing what it pushed and
//! what it derived it from.

mod oracle;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use tracing::{info, warn};

use oracle::{read_celestia_configs, read_evm_configs, read_funds, run_once, Config, Reading};

#[derive(Parser)]
#[command(about = "Keeps the Celestia IGP's destination gas configs current")]
struct Options {
    #[arg(long, default_value = "gas-oracle.toml")]
    config: String,
    #[arg(long, default_value = "0.0.0.0:3002")]
    listen: String,
    /// Push one round and exit, instead of running forever.
    #[arg(long)]
    once: bool,
}

/// What the page shows: the last round, and when the next one is due.
#[derive(Default)]
struct State {
    readings: Vec<Reading>,
    last_round: Option<u64>,
    next_round: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let options = Options::parse();

    let raw = std::fs::read_to_string(&options.config)
        .with_context(|| format!("reading {}", options.config))?;
    let config: Config = toml::from_str(&raw)?;
    // The public price feed rejects requests with no User-Agent.
    let http = reqwest::Client::builder()
        .user_agent(concat!("tee-hyperlane-gas-oracle/", env!("CARGO_PKG_VERSION")))
        .build()?;

    if options.once {
        for reading in run_once(&config, &http).await {
            println!("{}", serde_json::to_string_pretty(&reading)?);
        }
        return Ok(());
    }

    let state = Arc::new(Mutex::new(State::default()));
    let shared = Arc::new(Api { config: config.clone(), state: state.clone() });
    let updates = tokio::spawn(update_forever(config, http, state));

    let app = Router::new()
        .route("/", get(|| async { axum::response::Html(include_str!("dashboard.html")) }))
        .route("/api/readings", get(readings))
        .with_state(shared);

    let listener = tokio::net::TcpListener::bind(&options.listen).await?;
    info!(listen = %options.listen, "gas oracle listening");
    axum::serve(listener, app).await?;
    updates.abort();
    Ok(())
}

async fn update_forever(config: Config, http: reqwest::Client, state: Arc<Mutex<State>>) {
    let interval = Duration::from_secs(config.interval_secs);
    loop {
        let readings = run_once(&config, &http).await;
        for reading in &readings {
            match &reading.error {
                None => info!(
                    destination = %reading.name,
                    gas_price_wei = reading.gas_price_wei,
                    exchange_rate = reading.token_exchange_rate,
                    "pushed"
                ),
                // A failed round is not an outage: the IGP keeps the previous values, which
                // are stale but still charge something. The next round tries again.
                Some(error) => warn!(destination = %reading.name, %error, "round failed"),
            }
        }

        if let Ok(mut held) = state.lock() {
            held.readings = readings;
            held.last_round = Some(oracle::now());
            held.next_round = Some(oracle::now() + config.interval_secs);
        }
        tokio::time::sleep(interval).await;
    }
}

/// Config plus last-round state, so a request can both report what was pushed and go read
/// what the chains actually hold.
struct Api {
    config: Config,
    state: Arc<Mutex<State>>,
}

async fn readings(
    axum::extract::State(api): axum::extract::State<Arc<Api>>,
) -> Json<serde_json::Value> {
    // Chain reads shell out, so they run off the async pool.
    let for_chain = api.clone();
    let (onchain, funds) = tokio::task::spawn_blocking(move || {
        let mut all = read_celestia_configs(&for_chain.config);
        all.extend(read_evm_configs(&for_chain.config));
        (all, read_funds(&for_chain.config))
    })
    .await
    .unwrap_or_default();

    let held = api.state.lock().ok();
    let (readings, last, next) = match held {
        Some(held) => (
            serde_json::to_value(&held.readings).unwrap_or_default(),
            held.last_round,
            held.next_round,
        ),
        None => (serde_json::Value::Array(Vec::new()), None, None),
    };
    Json(serde_json::json!({
        "readings": readings,
        "onchain": onchain,
        "funds": funds,
        "lastRound": last,
        "nextRound": next,
        "intervalSecs": api.config.interval_secs,
    }))
}
