//! Talking to the dstack guest agent over its unix socket.
//!
//! This is the enclave's only outside contact, and it is local: dstack signs a 64-byte
//! `report_data` into a TDX quote and hands back the runtime event log. We send 32 bytes and
//! dstack zero-pads, which is why the circuits require the upper half to be zero.

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
        Self { socket, http: Client::unix() }
    }

    pub fn socket_path(&self) -> &str {
        &self.socket
    }

    /// Ask for a quote over exactly these 32 bytes.
    pub async fn get_quote(&self, report_data: [u8; 32]) -> Result<Quote> {
        let body = serde_json::json!({ "report_data": hex::encode(report_data) }).to_string();
        let raw: RawQuote = self.post("/GetQuote", body).await.context("dstack GetQuote")?;
        Ok(Quote { quote: raw.quote, event_log: raw.event_log })
    }

    /// dstack's view of this CVM. Used once at bootstrap to capture the measurements that
    /// go into `policy/identity.toml`.
    pub async fn info(&self) -> Result<serde_json::Value> {
        self.post("/Info", "{}".to_string()).await.context("dstack Info")
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
        anyhow::ensure!(status.is_success(), "dstack returned {status}: {}", String::from_utf8_lossy(&bytes));
        Ok(serde_json::from_slice(&bytes)?)
    }
}
