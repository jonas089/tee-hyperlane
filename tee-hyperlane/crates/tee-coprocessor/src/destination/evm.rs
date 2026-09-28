//! An EVM destination: `TeeDcapIsm.submitAttestation`, then `Mailbox.process` per message,
//! both sent with `cast`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use tee_node::state::IsmState;
use tracing::debug;

use super::{run, Batch, Delivery, Destination, Outcome};

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

    async fn sender(&self) -> Result<String> {
        run(
            "cast",
            &["wallet", "address", "--private-key", &Self::key()?],
        )
        .await
    }

    /// The next nonce counting transactions still in the mempool, so a pass that follows one
    /// whose transactions have not been mined yet does not reuse their nonces.
    async fn next_nonce(&self, sender: &str) -> Result<u64> {
        Ok(run(
            "cast",
            &[
                "nonce",
                sender,
                "--block",
                "pending",
                "--rpc-url",
                &self.rpc,
            ],
        )
        .await?
        .parse()?)
    }

    async fn send_process(&self, message: &[u8], nonce: u64) -> Result<String> {
        let gas = std::env::var("DELIVERY_GAS").unwrap_or_else(|_| "400000".into());
        self.send(
            &self.mailbox,
            "process(bytes,bytes)",
            &["0x", &format!("0x{}", hex::encode(message))],
            nonce,
            &["--gas-limit", &gas],
        )
        .await
    }

    /// Wait for `tx`; `Ok(false)` when it was mined and reverted.
    async fn landed(&self, tx: &str) -> Result<bool> {
        let receipt: Value = serde_json::from_str(
            &run(
                "cast",
                &[
                    "receipt",
                    tx,
                    "--rpc-url",
                    &self.rpc,
                    "--confirmations",
                    "1",
                    "--json",
                ],
            )
            .await?,
        )?;
        Ok(receipt["status"].as_str() == Some("0x1"))
    }

    /// Why `process` reverts for `message`, by replaying it as a call. The receipt of a
    /// reverted transaction carries no reason.
    async fn revert_reason(&self, message: &[u8], sender: &str) -> String {
        let replay = run(
            "cast",
            &[
                "call",
                &self.mailbox,
                "process(bytes,bytes)",
                "0x",
                &format!("0x{}", hex::encode(message)),
                "--from",
                sender,
                "--rpc-url",
                &self.rpc,
            ],
        )
        .await;
        match replay {
            Err(e) => e.to_string(),
            Ok(_) => "the transaction reverted, but replaying it now succeeds; likely out of gas \
                      (DELIVERY_GAS) or a state that has since changed"
                .into(),
        }
    }

    async fn is_delivered(&self, id: &str) -> Result<bool> {
        Ok(run(
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
        .await?
            == "true")
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

    async fn submit(&self, batch: &Batch) -> Result<Vec<Delivery>> {
        let state = run(
            "cast",
            &["call", &self.ism, "state()(bytes)", "--rpc-url", &self.rpc],
        )
        .await?;
        let advanced = batch.advanced(&hex::decode(state.trim_start_matches("0x"))?)?;
        let sender = self.sender().await?;
        let mut nonce = self.next_nonce(&sender).await?;

        let mut attestation = None;
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
                    debug!(tx, "submitted attestation");
                    attestation = Some(tx);
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

        // Sent back to back with explicit nonces, then confirmed together.
        let mut sent = Vec::new();
        let mut out = Vec::new();
        for (id, message) in batch.for_domain(self.domain) {
            if self.is_delivered(&id).await? {
                out.push(Delivery {
                    id,
                    message: message.clone(),
                    outcome: Outcome::AlreadyDelivered,
                });
                continue;
            }
            let tx = self.send_process(message, nonce).await?;
            debug!(id, tx, "delivering");
            sent.push((id, message.clone(), tx));
            nonce += 1;
        }

        if let Some(tx) = &attestation {
            anyhow::ensure!(self.landed(tx).await?, "attestation {tx} reverted");
        }
        for (id, message, tx) in sent {
            let outcome = if self.landed(&tx).await? {
                Outcome::Delivered { tx }
            } else {
                Outcome::Refused {
                    reason: self.revert_reason(&message, &sender).await,
                    tx: Some(tx),
                }
            };
            out.push(Delivery {
                id,
                message,
                outcome,
            });
        }
        Ok(out)
    }

    async fn deliver(&self, message: &[u8]) -> Result<Outcome> {
        let id = hex::encode(alloy_primitives::keccak256(message));
        if self.is_delivered(&id).await? {
            return Ok(Outcome::AlreadyDelivered);
        }
        let sender = self.sender().await?;
        let tx = self
            .send_process(message, self.next_nonce(&sender).await?)
            .await?;
        debug!(id, tx, "redelivering");
        Ok(if self.landed(&tx).await? {
            Outcome::Delivered { tx }
        } else {
            Outcome::Refused {
                reason: self.revert_reason(message, &sender).await,
                tx: Some(tx),
            }
        })
    }

    async fn delivered(&self, id: &str) -> Result<bool> {
        self.is_delivered(id).await
    }
}
