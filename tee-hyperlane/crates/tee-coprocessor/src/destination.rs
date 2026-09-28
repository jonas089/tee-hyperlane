//! Where a route delivers: read the ISM's state, submit an attestation, deliver its messages.
//!
//! Two kinds, EVM chains and Celestia. Both sign through the tools that deploy them - `cast`
//! and `celestia-appd` - rather than reimplementing signing: the relayer key is the least
//! sensitive thing in the system, since it can pay gas and stall a route but cannot make a
//! chain accept a message the enclave did not attest.
//!
//! Submitting is idempotent, so a batch interrupted anywhere can simply be submitted again:
//! an ISM already at the batch's state is not advanced twice, and a message already delivered
//! is skipped.

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::Value;
use tee_node::state::IsmState;
use tokio::process::Command;
use tracing::info;

#[async_trait]
pub trait Destination: Send + Sync {
    /// The ISM's trusted state: the only progress marker a route has, and it lives on chain,
    /// which is what makes restarting the same as continuing.
    async fn state(&self) -> Result<IsmState>;
    async fn submit(&self, batch: &Batch) -> Result<()>;
}

/// An attested batch, as the route stages it.
pub struct Batch {
    pub quote: String,
    pub event_log: String,
    /// The attested payload: `prev_state(116) new_state(116) tree(32) attested_at(8) count(8) ids`.
    pub payload: Vec<u8>,
    /// The message bytes, in tree order.
    pub messages: Vec<Vec<u8>>,
}

/// The ISM has moved past the state this batch starts from, so no retry can land it. The route
/// discards the batch and builds a new one from where the ISM actually is.
#[derive(Debug, thiserror::Error)]
#[error("stale batch: the ISM has moved past the state this batch starts from")]
pub struct Stale;

impl Batch {
    fn prev_state(&self) -> &[u8] {
        &self.payload[..116]
    }

    fn new_state(&self) -> &[u8] {
        &self.payload[116..232]
    }

    /// The messages addressed to `domain`, with their ids.
    fn for_domain(&self, domain: u32) -> impl Iterator<Item = (String, &Vec<u8>)> {
        self.messages.iter().filter_map(move |m| {
            let decoded = hyperlane_types::decode_hyperlane_message(m).ok()?;
            (decoded.destination == domain)
                .then(|| (hex::encode(alloy_primitives::keccak256(m)), m))
        })
    }

    /// Refuse a batch the ISM has moved past. `true` when the ISM is already at its new state,
    /// so only delivery is left to do.
    fn advanced(&self, state: &[u8]) -> Result<bool> {
        if state == self.new_state() {
            return Ok(true);
        }
        anyhow::ensure!(state == self.prev_state(), Stale);
        Ok(false)
    }
}

async fn run(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{program} {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// An EVM chain: `TeeDcapIsm.submitAttestation`, then `Mailbox.process` per message.
pub struct Evm {
    pub rpc: String,
    pub mailbox: String,
    pub domain: u32,
    pub ism: String,
}

impl Evm {
    fn key() -> Result<String> {
        std::env::var("EVM_PRIVATE_KEY").context("set EVM_PRIVATE_KEY for EVM destinations")
    }

    /// Send without waiting, so a batch's transactions go out back to back with explicit nonces
    /// and are confirmed together afterwards.
    async fn send(
        &self,
        to: &str,
        sig: &str,
        args: &[&str],
        nonce: u64,
        extra: &[&str],
    ) -> Result<String> {
        let key = Self::key()?;
        let nonce = nonce.to_string();
        let mut all = vec!["send", to, sig];
        all.extend_from_slice(args);
        all.extend_from_slice(&[
            "--rpc-url",
            &self.rpc,
            "--private-key",
            &key,
            "--nonce",
            &nonce,
            "--async",
        ]);
        all.extend_from_slice(extra);
        Ok(run("cast", &all).await?.trim_matches('"').to_string())
    }
}

#[async_trait]
impl Destination for Evm {
    async fn state(&self) -> Result<IsmState> {
        let hex_state = run(
            "cast",
            &["call", &self.ism, "state()(bytes)", "--rpc-url", &self.rpc],
        )
        .await?;
        Ok(IsmState::decode(&hex::decode(
            hex_state.trim_start_matches("0x"),
        )?)?)
    }

    async fn submit(&self, batch: &Batch) -> Result<()> {
        let state = run(
            "cast",
            &["call", &self.ism, "state()(bytes)", "--rpc-url", &self.rpc],
        )
        .await?;
        let advanced = batch.advanced(&hex::decode(state.trim_start_matches("0x"))?)?;
        let sender = run(
            "cast",
            &["wallet", "address", "--private-key", &Self::key()?],
        )
        .await?;
        let mut nonce: u64 = run("cast", &["nonce", &sender, "--rpc-url", &self.rpc])
            .await?
            .parse()?;
        let mut pending = Vec::new();

        if !advanced {
            let quote = format!("0x{}", batch.quote.trim_start_matches("0x"));
            let payload = format!("0x{}", hex::encode(&batch.payload));
            match self
                .send(
                    &self.ism,
                    "submitAttestation(bytes,bytes)",
                    &[&quote, &payload],
                    nonce,
                    &[],
                )
                .await
            {
                Ok(tx) => {
                    info!(tx, "submitted attestation");
                    pending.push(tx);
                    nonce += 1;
                }
                // Automata refuses with a four-letter code, such as "TCBR" for collateral missing
                // from this chain's PCCS. The ISM can say what it means; the code alone cannot.
                Err(e) => {
                    let text = e.to_string();
                    let Some(code) = text
                        .split("QuoteRejected(")
                        .nth(1)
                        .and_then(|r| r.split(')').next())
                    else {
                        return Err(e);
                    };
                    let why = run(
                        "cast",
                        &[
                            "call",
                            &self.ism,
                            "describeQuoteError(bytes)(string)",
                            code,
                            "--rpc-url",
                            &self.rpc,
                        ],
                    )
                    .await
                    .unwrap_or_default();
                    anyhow::bail!("the verifier refused the quote ({code}): {why}");
                }
            }
        }

        let gas = std::env::var("DELIVERY_GAS").unwrap_or_else(|_| "400000".into());
        for (id, message) in batch.for_domain(self.domain) {
            let delivered = run(
                "cast",
                &[
                    "call",
                    &self.mailbox,
                    "delivered(bytes32)(bool)",
                    &format!("0x{id}"),
                    "--rpc-url",
                    &self.rpc,
                ],
            )
            .await?;
            if delivered == "true" {
                continue;
            }
            let tx = self
                .send(
                    &self.mailbox,
                    "process(bytes,bytes)",
                    &["0x", &format!("0x{}", hex::encode(message))],
                    nonce,
                    &["--gas-limit", &gas],
                )
                .await?;
            info!(id, tx, "delivering");
            pending.push(tx);
            nonce += 1;
        }

        for tx in pending {
            let receipt: Value = serde_json::from_str(
                &run(
                    "cast",
                    &[
                        "receipt",
                        &tx,
                        "--rpc-url",
                        &self.rpc,
                        "--confirmations",
                        "1",
                        "--json",
                    ],
                )
                .await?,
            )?;
            anyhow::ensure!(
                receipt["status"].as_str() == Some("0x1"),
                "transaction {tx} reverted"
            );
        }
        Ok(())
    }
}

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
