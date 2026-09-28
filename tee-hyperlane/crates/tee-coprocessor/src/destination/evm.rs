//! An EVM destination: `TeeDcapIsm.submitAttestation`, then `Mailbox.process` per message,
//! both sent with `cast`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use tee_node::state::IsmState;
use tracing::info;

use super::{run, Batch, Destination};

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
