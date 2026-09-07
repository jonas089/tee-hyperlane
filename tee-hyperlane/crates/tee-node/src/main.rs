//! The enclave process.
//!
//! One endpoint that matters. It takes everything it needs in the request body, verifies all
//! of it, asks dstack to sign the result, and returns. No state is kept between calls and
//! nothing is written to disk, so restarting this process loses nothing.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use std::sync::Arc;
use tee_node::attest::{build_attested_update, report_data_for, AttestRequest};
use tee_node::dstack::DstackClient;
use tracing::{error, info};

#[derive(Serialize)]
struct AttestResponse {
    /// Hex-encoded TDX quote over `sha256(payload)`.
    quote: String,
    /// dstack's runtime event log, as JSON text.
    event_log: String,
    /// The canonical attested payload, hex. Both SP1 programs re-derive its hash.
    payload: String,
    /// The state this update moves the ISM to, hex.
    new_state: String,
    /// Message ids this update authorises.
    message_ids: Vec<String>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ErrorResponse>)>;

fn reject(status: StatusCode, error: impl std::fmt::Display) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: error.to_string() }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let dstack = Arc::new(DstackClient::from_env());
    info!(socket = dstack.socket_path(), "dstack guest agent");

    let app = Router::new()
        .route("/attest", post(attest))
        .route("/identity", get(identity))
        .route("/health", get(|| async { "ok" }))
        .with_state(dstack);

    let addr = std::env::var("LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(%addr, "tee-node listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Verify one step and attest it.
async fn attest(
    State(dstack): State<Arc<DstackClient>>,
    Json(mut request): Json<AttestRequest>,
) -> ApiResult<AttestResponse> {
    let (update, payload) = build_attested_update(&mut request).map_err(|e| {
        // A rejection here is the enclave doing its job, not an outage: some part of what
        // the coprocessor supplied did not check out.
        error!(error = %e, "rejected");
        reject(StatusCode::BAD_REQUEST, e)
    })?;

    let quote = dstack
        .get_quote(report_data_for(&update))
        .await
        .map_err(|e| reject(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    info!(
        height = update.new_state.height,
        messages = update.message_ids.len(),
        "attested"
    );
    Ok(Json(AttestResponse {
        quote: quote.quote,
        event_log: quote.event_log,
        payload: hex::encode(&payload),
        new_state: hex::encode(tee_attestation::encode_ism_state(&update.new_state)),
        message_ids: update.message_ids.iter().map(hex::encode).collect(),
    }))
}

/// This enclave's own measurements, read once at bootstrap to fill in
/// `tee-circuit/policy/identity.toml`.
///
/// Returns a quote over zeroes plus the event log, which together carry every value the
/// identity policy pins.
async fn identity(State(dstack): State<Arc<DstackClient>>) -> ApiResult<serde_json::Value> {
    let quote = dstack
        .get_quote([0u8; 32])
        .await
        .map_err(|e| reject(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let info = dstack.info().await.unwrap_or(serde_json::Value::Null);
    Ok(Json(serde_json::json!({
        "quote": quote.quote,
        "event_log": quote.event_log,
        "info": info,
        "note": "feed this to `xtask identity` to produce policy/identity.toml",
    })))
}
