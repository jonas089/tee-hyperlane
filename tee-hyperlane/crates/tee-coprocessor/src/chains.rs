//! Submitting to a destination chain.
//!
//! Both chains run the same protocol - advance the state, authorise the batch, deliver each
//! message - so this is one shape with two backends. Signing is delegated to `cast` and
//! `celestia-appd` rather than reimplemented: they are already required to deploy the bridge,
//! and a relayer key is the least sensitive thing in the system. It pays gas and can stall
//! the bridge; it cannot make either chain accept a message the enclave did not attest.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use base64::Engine as _;
use tracing::info;

use crate::config::ChainConfig;

pub struct Destination {
    chain: ChainConfig,
    ism: String,
}

impl Destination {
    pub fn new(chain: ChainConfig, ism: String) -> Self {
        Self { chain, ism }
    }

    /// Run one proved batch to completion: update, authorise, deliver.
    ///
    /// Ordering is forced by the ISM: `submitMessages` checks the batch against the *current*
    /// root, so the state has to move first.
    pub fn submit(&self, proved: &Path) -> Result<()> {
        let script = match self.chain {
            ChainConfig::Celestia { .. } => "submit-celestia.sh",
            _ => "submit-evm.sh",
        };
        let path = deploy_dir().join(script);
        info!(script, ism = %self.ism, "submitting");

        let mut command = Command::new(&path);
        command
            .arg(proved)
            .env("TEE_ISM", &self.ism)
            .env("CELESTIA_ISM", &self.ism);

        // Which chain to send to comes from the route, so a second EVM destination is config.
        match &self.chain {
            ChainConfig::Ethereum { execution_rpc, mailbox, .. } => {
                command.env("EVM_RPC", execution_rpc).env("MAILBOX", mailbox);
            }
            ChainConfig::EthereumL2 { l2_rpc, .. } => {
                command.env("EVM_RPC", l2_rpc);
            }
            ChainConfig::Celestia { rpc, mailbox_id, .. } => {
                command.env("CELESTIA_RPC", rpc).env("CELESTIA_MAILBOX", mailbox_id);
            }
        }

        let status = command
            .status()
            .with_context(|| format!("running {}", path.display()))?;

        anyhow::ensure!(status.success(), "{script} exited with {status}");
        Ok(())
    }
}

fn deploy_dir() -> std::path::PathBuf {
    std::env::var("TEE_HYPERLANE_DEPLOY_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("deploy"))
}

/// Read the ISM's current trusted state, hex-encoded.
///
/// This is the only progress marker the relayer has, and it lives on chain — which is what
/// makes restarting the same as continuing.
pub fn read_ism_state(chain: &ChainConfig, ism: &str) -> Result<String> {
    let output = match chain {
        ChainConfig::Celestia { rpc, .. } => Command::new(celestia_appd())
            .args(["query", "zkism", "ism", ism, "--node", rpc, "-o", "json"])
            .output()
            .context("celestia-appd query zkism ism")?,
        ChainConfig::Ethereum { execution_rpc, .. }
        | ChainConfig::EthereumL2 { l2_rpc: execution_rpc, .. } => Command::new("cast")
            .args(["call", ism, "state()(bytes)", "--rpc-url", execution_rpc])
            .output()
            .context("cast call state()")?,
    };
    anyhow::ensure!(
        output.status.success(),
        "reading ISM state failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8(output.stdout)?;

    Ok(match chain {
        ChainConfig::Celestia { .. } => {
            let parsed: serde_json::Value = serde_json::from_str(&text)?;
            let encoded = parsed["ism"]["state"]
                .as_str()
                .context("ism.state missing from query output")?;
            // celestia-appd renders `bytes` fields as base64; everything downstream expects
            // hex, and the two are easy to mistake for one another.
            let raw = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .context("ism.state is not base64")?;
            format!("0x{}", hex::encode(raw))
        }
        _ => text.trim().to_string(),
    })
}

fn celestia_appd() -> String {
    std::env::var("APPD").unwrap_or_else(|_| "celestia-appd".to_string())
}
