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
use tee_node::attest::{build_attested_update, AttestRequest};
use tracing::{error, info};

#[derive(Serialize)]
struct AttestResponse {
    /// Hex-encoded TDX quote over `sha256(payload)`.
    quote: String,
    /// dstack's runtime event log, as JSON text.
    event_log: String,
    /// The canonical attested payload, hex. Both ISMs check its sha256 against report_data.
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
    (
        status,
        Json(ErrorResponse {
            error: error.to_string(),
        }),
    )
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
        // After the routes, not before: a layer only wraps what was added before it, so this
        // sitting on top of an empty router silently did nothing. axum's 2 MB default is too
        // small for an Eden request, which carries a Celestia block's shares plus a witness
        // per re-executed block. The body is verified, not trusted, so the limit only bounds
        // memory.
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
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
    Json(request): Json<AttestRequest>,
) -> ApiResult<AttestResponse> {
    let (update, payload) = build_attested_update(request).map_err(|e| {
        // A rejection here is the enclave doing its job, not an outage: some part of what
        // the coprocessor supplied did not check out.
        error!(error = %e, "rejected");
        reject(StatusCode::BAD_REQUEST, e)
    })?;

    let quote = dstack
        .get_quote(update.hash())
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
        new_state: hex::encode(update.new_state.encode()),
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
        "note": "feed this to `circuit-tool identity` to pin enclave-identity.toml",
    })))
}

/// The dstack guest agent, which is what turns 32 bytes of report data into a TDX quote. It is
/// the only thing the enclave talks to, over a local unix socket.
mod dstack {
    //! Talking to the dstack guest agent over its unix socket.
    //!
    //! This is the enclave's only outside contact, and it is local: dstack signs a 64-byte
    //! `report_data` into a TDX quote and hands back the runtime event log. We send 32 bytes and
    //! dstack zero-pads, which is why the ISMs require the upper half to be zero.

    use anyhow::{Context, Result};
    use http_body_util::BodyExt;
    use hyper_util::client::legacy::Client;
    use hyperlocal::{UnixClientExt, UnixConnector, Uri};
    use serde::{Deserialize, Serialize};

    /// Where the guest agent listens, in the order dstack itself tries.
    const SOCKET_PATHS: &[&str] = &[
        "/var/run/dstack.sock",
        "/run/dstack.sock",
        "/var/run/dstack/dstack.sock",
        "/run/dstack/dstack.sock",
    ];

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Quote {
        /// Hex-encoded TDX quote.
        pub quote: String,
        /// The runtime event log, as JSON text.
        pub event_log: String,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct RawQuote {
        quote: String,
        event_log: String,
    }

    pub struct DstackClient {
        socket: String,
        http: Client<UnixConnector, String>,
    }

    impl DstackClient {
        /// Resolve the socket the way dstack's own SDK does, with an env override so the
        /// service can be exercised against the simulator off real hardware.
        pub fn from_env() -> Self {
            let socket = std::env::var("DSTACK_SOCKET").ok().unwrap_or_else(|| {
                SOCKET_PATHS
                    .iter()
                    .find(|p| std::path::Path::new(p).exists())
                    .unwrap_or(&SOCKET_PATHS[0])
                    .to_string()
            });
            Self {
                socket,
                http: Client::unix(),
            }
        }

        pub fn socket_path(&self) -> &str {
            &self.socket
        }

        /// Ask for a quote over exactly these 32 bytes.
        pub async fn get_quote(&self, report_data: [u8; 32]) -> Result<Quote> {
            let body = serde_json::json!({ "report_data": hex::encode(report_data) }).to_string();
            let raw: RawQuote = self
                .post("/GetQuote", body)
                .await
                .context("dstack GetQuote")?;
            Ok(Quote {
                quote: raw.quote,
                event_log: raw.event_log,
            })
        }

        /// dstack's view of this CVM. Used once at bootstrap to capture the measurements that
        /// go into `policy/identity.toml`.
        pub async fn info(&self) -> Result<serde_json::Value> {
            self.post("/Info", "{}".to_string())
                .await
                .context("dstack Info")
        }

        async fn post<T: for<'de> Deserialize<'de>>(&self, path: &str, body: String) -> Result<T> {
            let uri: hyper::Uri = Uri::new(&self.socket, path).into();
            let request = hyper::Request::builder()
                .method("POST")
                .uri(uri)
                .header("Host", "dstack")
                .header("Content-Type", "application/json")
                .body(body)?;
            let response = self.http.request(request).await?;
            let status = response.status();
            let bytes = response.into_body().collect().await?.to_bytes();
            anyhow::ensure!(
                status.is_success(),
                "dstack returned {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
            Ok(serde_json::from_slice(&bytes)?)
        }
    }
}
use dstack::DstackClient;
