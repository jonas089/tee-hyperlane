//! The Celestia destination: `x/teeism` submit-attestation with fresh Intel collateral carried
//! in the transaction, then `hyperlane mailbox process` per message, both via `celestia-appd`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::Value;
use tee_node::state::IsmState;
use tracing::info;

use super::{run, Batch, Destination};

/// Celestia: `x/teeism` submit-attestation, with fresh Intel collateral carried in the
/// transaction, then `hyperlane mailbox process` per message.
pub struct Celestia {
    pub rpc: String,
    pub mailbox: String,
    pub domain: u32,
    pub ism: String,
    pub chain_id: String,
    pub key: String,
    pub home: String,
}

impl Celestia {
    fn appd() -> String {
        std::env::var("APPD").unwrap_or_else(|_| "celestia-appd".into())
    }

    async fn query(&self, args: &[&str]) -> Result<Value> {
        let mut all = vec!["query"];
        all.extend_from_slice(args);
        all.extend_from_slice(&["--node", &self.rpc, "-o", "json"]);
        Ok(serde_json::from_str(&run(&Self::appd(), &all).await?)?)
    }

    /// Broadcast and wait for inclusion, since `sync` only means the node accepted it.
    async fn send(&self, args: &[&str]) -> Result<()> {
        let mut all = vec!["tx"];
        all.extend_from_slice(args);
        all.extend_from_slice(&[
            "--from",
            &self.key,
            "--home",
            &self.home,
            "--keyring-backend",
            "test",
            "--chain-id",
            &self.chain_id,
            "--node",
            &self.rpc,
            "--fees",
            "200000utia",
            "--gas",
            "2000000",
            "-y",
            "-o",
            "json",
        ]);
        let sent: Value = serde_json::from_str(&run(&Self::appd(), &all).await?)?;
        anyhow::ensure!(
            sent["code"].as_u64().unwrap_or(0) == 0,
            "rejected at broadcast: {}",
            sent["raw_log"]
        );
        let hash = sent["txhash"].as_str().context("no txhash")?.to_string();
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Ok(tx) = self.query(&["tx", &hash]).await {
                anyhow::ensure!(
                    tx["code"].as_u64() == Some(0),
                    "{hash} failed: {}",
                    tx["raw_log"]
                );
                info!(tx = hash, "included");
                return Ok(());
            }
        }
        anyhow::bail!("{hash} was accepted but not included within 60s")
    }
}

#[async_trait]
impl Destination for Celestia {
    async fn state(&self) -> Result<IsmState> {
        let ism = self.query(&["teeism", "ism", &self.ism]).await?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(ism["ism"]["state"].as_str().context("ism.state")?)?;
        Ok(IsmState::decode(&raw)?)
    }

    async fn submit(&self, batch: &Batch) -> Result<()> {
        let ism = self.query(&["teeism", "ism", &self.ism]).await?;
        let state = base64::engine::general_purpose::STANDARD
            .decode(ism["ism"]["state"].as_str().context("ism.state")?)?;
        if !batch.advanced(&state)? {
            // The module verifies the quote itself, against collateral it is handed rather than
            // collateral stored on chain, so nothing here expires.
            let dir = tempfile::tempdir()?;
            let quote = dir.path().join("quote.hex");
            let collateral = dir.path().join("collateral.json");
            std::fs::write(&quote, &batch.quote)?;
            let bin =
                std::env::var("COLLATERAL_BIN").unwrap_or_else(|_| "teeism-collateral".into());
            run(
                &bin,
                &[
                    "-quote",
                    quote.to_str().unwrap(),
                    "-out",
                    collateral.to_str().unwrap(),
                ],
            )
            .await?;
            let bundle = dir.path().join("submit.json");
            std::fs::write(
                &bundle,
                serde_json::to_vec(&serde_json::json!({
                    "quote": batch.quote,
                    "event_log": format!("0x{}", hex::encode(batch.event_log.as_bytes())),
                    "payload": format!("0x{}", hex::encode(&batch.payload)),
                    "collateral": serde_json::from_slice::<Value>(&std::fs::read(&collateral)?)?,
                }))?,
            )?;
            self.send(&[
                "teeism",
                "submit-attestation",
                &self.ism,
                bundle.to_str().unwrap(),
            ])
            .await?;
        }
        for (id, message) in batch.for_domain(self.domain) {
            let delivered = self
                .query(&["hyperlane", "delivered", &self.mailbox, &format!("0x{id}")])
                .await?;
            if delivered["delivered"].as_bool() == Some(true) {
                continue;
            }
            info!(id, "delivering");
            self.send(&[
                "hyperlane",
                "mailbox",
                "process",
                &self.mailbox,
                "0x",
                &format!("0x{}", hex::encode(message)),
            ])
            .await?;
        }
        Ok(())
    }
}
