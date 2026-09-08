//! Asking the enclave to attest.
//!
//! Everything sent here is public and everything sent here is re-verified inside. A wrong
//! answer from any RPC we gathered from produces a rejected attestation, not a bad root, so
//! this client needs no trust of its own.

use anyhow::{Context, Result};
use serde::Deserialize;

pub struct EnclaveClient {
    url: String,
    http: reqwest::Client,
}

/// What the enclave returns once it has verified and signed.
#[derive(Debug, Clone, Deserialize)]
pub struct Attestation {
    /// Hex TDX quote over sha256(payload).
    pub quote: String,
    /// dstack runtime event log, JSON text.
    pub event_log: String,
    /// Canonical attested payload, hex.
    pub payload: String,
    /// The ISM state this update moves to, hex.
    pub new_state: String,
    pub message_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct EnclaveError {
    error: String,
}

impl EnclaveClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    pub async fn health(&self) -> Result<String> {
        Ok(self
            .http
            .get(format!("{}/health", self.url))
            .send()
            .await?
            .text()
            .await?)
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

/// Intel's collateral for a quote, fetched from Phala's PCCS mirror.
///
/// Untrusted: the circuit checks every signature in it against Intel's root CA, which is
/// baked into the guest ELF.
pub async fn fetch_collateral(quote_hex: &str) -> Result<dcap_qvl::QuoteCollateralV3> {
    let quote = hex::decode(quote_hex.trim_start_matches("0x"))?;
    let client = dcap_qvl::collateral::CollateralClient::with_default_http(
        "https://pccs.phala.network/sgx/certification/v4/",
    )
    .map_err(|e| anyhow::anyhow!("PCCS client: {e:?}"))?;
    client
        .fetch(&quote)
        .await
        .map_err(|e| anyhow::anyhow!("PCCS collateral: {e:?}"))
}
