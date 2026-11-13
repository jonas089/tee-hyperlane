//! Delivers to Celestia: submits attestations to `x/teeism` and processes messages, using `celestia-appd`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::Value;
use tee_node::state::IsmState;
use tracing::debug;

use super::{run, Batch, Delivery, Destination, Outcome, Submitted, Wallet};

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
    /// What the node charges per gas, as `--gas-prices` takes it: `0.004utia`.
    pub gas_price: String,
}

/// Why the chain refused the transaction at simulation, if that is what the CLI's error says.
///
/// `--gas auto` simulates first. A message the chain will not execute fails there, and the
/// SDK wraps every handler error the same way; anything else (a node that is down, a fee or
/// sequence problem) is an error to retry, not a refusal.
pub fn simulation_refusal(error: &str) -> Option<String> {
    let at = error.find("failed to execute message")?;
    Some(format!("rejected at simulation: {}", error[at..].trim()))
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
    async fn send(&self, args: &[&str]) -> Result<String> {
        match self.send_checked(args).await? {
            Ok(hash) => Ok(hash),
            Err(Refusal { reason, .. }) => Err(anyhow::anyhow!(reason)),
        }
    }

    /// As `send`, but a transaction the chain refused comes back as a `Refusal` rather than an
    /// error, so a message the chain will not take can be told from a node that is down.
    ///
    /// Gas is what the node simulates plus a margin, and the fee is that gas at the configured
    /// price, so an attestation costs what it costs and never runs out of room. A message the
    /// chain will not execute fails the simulation, before any fee is paid.
    async fn send_checked(&self, args: &[&str]) -> Result<std::result::Result<String, Refusal>> {
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
            "--gas",
            "auto",
            "--gas-adjustment",
            super::GAS_ADJUSTMENT,
            "--gas-prices",
            &self.gas_price,
            "-y",
            "-o",
            "json",
        ]);
        let out = match run(&Self::appd(), &all).await {
            Ok(out) => out,
            Err(e) => {
                return Ok(match simulation_refusal(&e.to_string()) {
                    Some(reason) => Err(Refusal { tx: None, reason }),
                    None => return Err(e),
                })
            }
        };
        let sent: Value = serde_json::from_str(&out)?;
        let code = sent["code"].as_u64().unwrap_or(0);
        // 32 is a stale account sequence: a race with our own earlier transaction, not
        // anything wrong with this one.
        anyhow::ensure!(code != 32, "rejected at broadcast: {}", sent["raw_log"]);
        if code != 0 {
            return Ok(Err(Refusal {
                tx: None,
                reason: format!("rejected at broadcast (code {code}): {}", sent["raw_log"]),
            }));
        }
        let hash = sent["txhash"].as_str().context("no txhash")?.to_string();
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Ok(tx) = self.query(&["tx", &hash]).await {
                let code = tx["code"].as_u64().unwrap_or(0);
                if code != 0 {
                    return Ok(Err(Refusal {
                        reason: format!("{hash} failed (code {code}): {}", tx["raw_log"]),
                        tx: Some(hash),
                    }));
                }
                debug!(tx = hash, "included");
                return Ok(Ok(hash));
            }
        }
        anyhow::bail!("{hash} was accepted but not included within 60s")
    }

    async fn is_delivered(&self, id: &str) -> Result<bool> {
        let delivered = self
            .query(&["hyperlane", "delivered", &self.mailbox, &format!("0x{id}")])
            .await?;
        Ok(delivered["delivered"].as_bool() == Some(true))
    }

    async fn process(&self, message: &[u8], metadata: &[u8]) -> Result<Outcome> {
        let sent = self
            .send_checked(&[
                "hyperlane",
                "mailbox",
                "process",
                &self.mailbox,
                &format!("0x{}", hex::encode(metadata)),
                &format!("0x{}", hex::encode(message)),
            ])
            .await?;
        Ok(match sent {
            Ok(tx) => Outcome::Delivered { tx },
            Err(Refusal { tx, reason }) => Outcome::Refused { tx, reason },
        })
    }
}

struct Refusal {
    tx: Option<String>,
    reason: String,
}

#[async_trait]
impl Destination for Celestia {
    async fn state(&self) -> Result<IsmState> {
        let ism = self.query(&["teeism", "ism", &self.ism]).await?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(ism["ism"]["state"].as_str().context("ism.state")?)?;
        Ok(IsmState::decode(&raw)?)
    }

    async fn submit(&self, batch: &Batch) -> Result<Submitted> {
        let ism = self.query(&["teeism", "ism", &self.ism]).await?;
        let state = base64::engine::general_purpose::STANDARD
            .decode(ism["ism"]["state"].as_str().context("ism.state")?)?;
        let mut attestation_tx = None;
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
            attestation_tx = Some(
                self.send(&[
                    "teeism",
                    "submit-attestation",
                    &self.ism,
                    bundle.to_str().unwrap(),
                ])
                .await?,
            );
        }
        let mut out = Vec::new();
        for (id, message, metadata) in batch.for_domain(self.domain) {
            let outcome = if self.is_delivered(&id).await? {
                Outcome::AlreadyDelivered
            } else {
                debug!(id, "delivering");
                self.process(message, metadata).await?
            };
            out.push(Delivery {
                id,
                message: message.clone(),
                metadata: metadata.clone(),
                outcome,
            });
        }
        Ok(Submitted {
            attestation_tx,
            deliveries: out,
        })
    }

    async fn deliver(&self, message: &[u8], metadata: &[u8]) -> Result<Outcome> {
        let id = hex::encode(alloy_primitives::keccak256(message));
        if self.is_delivered(&id).await? {
            return Ok(Outcome::AlreadyDelivered);
        }
        debug!(id, "redelivering");
        self.process(message, metadata).await
    }

    async fn delivered(&self, id: &str) -> Result<bool> {
        self.is_delivered(id).await
    }

    async fn tree(&self) -> Result<([u8; 32], u32)> {
        let ism = self.query(&["teeism", "ism", &self.ism]).await?;
        // Proto bytes are base64 in JSON, and absent while empty: before the first attestation.
        let root = match ism["ism"]["tree_root"].as_str() {
            Some(b64) if !b64.is_empty() => base64::engine::general_purpose::STANDARD
                .decode(b64)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("ism.tree_root is not 32 bytes"))?,
            _ => [0u8; 32],
        };
        let count = &ism["ism"]["tree_count"];
        let count = match count.as_u64() {
            Some(n) => n,
            None => count
                .as_str()
                .unwrap_or("0")
                .parse()
                .context("ism.tree_count")?,
        };
        Ok((root, u32::try_from(count).context("ism.tree_count")?))
    }

    async fn root_attested(&self, root: [u8; 32]) -> Result<bool> {
        let roots = self.query(&["teeism", "roots", &self.ism]).await?;
        let want = hex::encode(root);
        Ok(roots["roots"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|r| r.trim_start_matches("0x").eq_ignore_ascii_case(&want)))
    }

    async fn wallet(&self) -> Result<Wallet> {
        let address = run(
            &Self::appd(),
            &[
                "keys",
                "show",
                &self.key,
                "-a",
                "--home",
                &self.home,
                "--keyring-backend",
                "test",
            ],
        )
        .await?;
        let reply = self.query(&["bank", "balance", &address, "utia"]).await?;
        let amount = reply["balance"]["amount"]
            .as_str()
            .context("no utia balance in the reply")?
            .parse()?;
        Ok(Wallet {
            address,
            balance: amount,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::simulation_refusal;

    /// A message the chain will not execute fails the simulation with the SDK's handler
    /// wrapper, and comes back as a refusal naming the chain's reason.
    #[test]
    fn a_failed_simulation_is_a_refusal() {
        let e = "celestia-appd tx: Error: rpc error: code = Unknown desc = failed to execute message; message index: 0: ism 0x2b: root not attested: unknown request";
        let reason = simulation_refusal(e).unwrap();
        assert!(
            reason.starts_with("rejected at simulation: failed to execute message"),
            "{reason}"
        );
        assert!(reason.contains("root not attested"), "{reason}");
    }

    /// A node that is down, or a fee the node will not take, is not the chain refusing the
    /// message: the caller retries.
    #[test]
    fn other_errors_are_not_refusals() {
        assert!(simulation_refusal("celestia-appd tx: Error: post failed: Post \"http://localhost:26657\": dial tcp: connection refused").is_none());
        assert!(simulation_refusal(
            "celestia-appd tx: Error: insufficient fees; got: 10utia required: 4000utia"
        )
        .is_none());
    }
}
