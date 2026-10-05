//! Gathers what the enclave needs to verify Arbitrum: a feed message its sequencer signed, the
//! header of the block it produced, and tree proofs.
//!
//! The sequencer feed is a websocket the sequencer itself serves. A background task keeps the
//! newest signed messages; each tick attests the newest one whose block the RPC can prove.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt;
use serde_json::{json, Value};
use tee_node::chains::l2::arbitrum::{
    signing_hash, FeedMessage, Header, ARBITRUM, SEQUENCER, TREE_SLOT,
};
use tee_node::state::IsmState;
use tracing::{debug, info, warn};

use super::sequenced::{Config, Recent, Sequenced, SignedHead, TRIES};
use crate::origin::{Indexer, Message, Step};

/// One feed connection per process, however many routes start from Arbitrum.
static FEED: OnceLock<Arc<Recent<FeedMessage>>> = OnceLock::new();

pub struct Arbitrum {
    chain: Sequenced,
    url: String,
}

impl Arbitrum {
    pub fn new(config: Config) -> Result<Self> {
        anyhow::ensure!(
            config.domain == ARBITRUM.domain,
            "chain `arbitrum` has domain {}, but the enclave attests Arbitrum as {}",
            config.domain,
            ARBITRUM.domain
        );
        let url = config
            .feed
            .clone()
            .context("an Arbitrum origin needs `feed`, the sequencer feed's websocket")?;
        Ok(Self {
            chain: Sequenced::new(&ARBITRUM, TREE_SLOT, &config),
            url,
        })
    }

    /// The feed's newest messages, connecting on first use: that is inside the runtime, which
    /// building the config is not.
    fn feed(&self) -> &Recent<FeedMessage> {
        FEED.get_or_init(|| listen(self.url.clone()))
    }

    /// The newest signed messages above `height`, each with its block's header.
    async fn heads(&self, height: u64) -> Result<Vec<SignedHead>> {
        let mut heads = Vec::new();
        for message in self.feed().above(0, TRIES) {
            let (rlp, _) = match self.chain.rpc().header_rlp(message.block_hash).await {
                Ok(h) => h,
                Err(e) => {
                    debug!(block = %message.block_hash, error = %e, "the RPC does not have this block yet");
                    continue;
                }
            };
            let header = Header::decode(&rlp)?;
            if header.number <= height {
                break;
            }
            heads.push(SignedHead {
                height: header.number,
                state_root: header.state_root,
                timestamp: header.timestamp,
                input: json!({ "message": message, "header_rlp": format!("0x{}", hex::encode(rlp)) }),
            });
        }
        Ok(heads)
    }
}

#[async_trait]
impl Indexer for Arbitrum {
    async fn gather(&self, trusted: &IsmState) -> Result<Step> {
        self.feed().check_alive("arbitrum sequencer feed")?;
        let heads = self.heads(trusted.height).await?;
        self.chain.step(trusted, &heads).await
    }

    async fn index(&self, from: u64, to: u64) -> Result<Vec<Message>> {
        self.chain.index(from, to).await
    }

    async fn bootstrap(&self, identity: [u8; 32], _height: Option<u64>) -> Result<IsmState> {
        self.feed().first(Duration::from_secs(60)).await?;
        let head = self
            .heads(0)
            .await?
            .into_iter()
            .next()
            .context("no signed Arbitrum block the RPC has")?;
        Ok(self.chain.genesis(&head, identity))
    }
}

/// Keep the feed's newest signed messages, reconnecting whenever it drops.
fn listen(url: String) -> Arc<Recent<FeedMessage>> {
    let recent = Arc::new(Recent::default());
    let out = recent.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = read_feed(&url, &out).await {
                warn!(error = %format!("{e:#}"), "arbitrum feed dropped; reconnecting");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    recent
}

async fn read_feed(url: &str, recent: &Recent<FeedMessage>) -> Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url.into_client_request()?;
    // Version 2 is the one that carries `signatureV2` and the block hash.
    request
        .headers_mut()
        .insert("Arbitrum-Feed-Client-Version", "2".parse()?);
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("connecting to the arbitrum feed")?;
    info!("reading the arbitrum sequencer feed");
    while let Some(frame) = socket.next().await {
        let frame = frame?;
        if !frame.is_text() {
            continue;
        }
        let batch: Value = serde_json::from_str(frame.to_text()?)?;
        for raw in batch["messages"].as_array().into_iter().flatten() {
            match feed_message(raw) {
                Ok(m) => recent.insert(m.sequence_number, m),
                Err(e) => debug!(error = %e, "skipping a feed message"),
            }
        }
    }
    anyhow::bail!("the feed closed")
}

/// A feed message as the enclave's input, if the pinned sequencer signed it.
fn feed_message(raw: &Value) -> Result<FeedMessage> {
    let b64 = |v: &Value| -> Result<alloy_primitives::Bytes> {
        Ok(match v.as_str() {
            Some(s) => base64::engine::general_purpose::STANDARD.decode(s)?.into(),
            None => Default::default(),
        })
    };
    let outer = &raw["message"];
    let header = &outer["message"]["header"];
    let message = FeedMessage {
        sequence_number: raw["sequenceNumber"].as_u64().context("sequenceNumber")?,
        block_hash: raw["blockHash"].as_str().context("no blockHash")?.parse()?,
        block_metadata: b64(&raw["blockMetadata"])?,
        delayed_messages_read: outer["delayedMessagesRead"]
            .as_u64()
            .context("delayedMessagesRead")?,
        kind: header["kind"].as_u64().context("kind")? as u8,
        sender: header["sender"].as_str().context("sender")?.parse()?,
        l1_block_number: header["blockNumber"].as_u64().context("blockNumber")?,
        timestamp: header["timestamp"].as_u64().context("timestamp")?,
        request_id: header["requestId"].as_str().map(str::parse).transpose()?,
        l1_base_fee: match &header["baseFeeL1"] {
            Value::Null => None,
            v => Some(v.to_string().trim_matches('"').parse()?),
        },
        l2_msg: b64(&outer["message"]["l2Msg"])?,
        signature: b64(&raw["signatureV2"])?,
    };
    tee_node::chains::l2::sequencer::verify(signing_hash(&message), &message.signature, SEQUENCER)?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message exactly as the feed sends it, checked against the sequencer the enclave pins.
    #[test]
    fn a_raw_feed_message_is_read_and_verified() {
        let raw: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/arbitrum_feed_raw.json"
        ))
        .unwrap();
        let m = feed_message(&raw).unwrap();
        assert_eq!(m.sequence_number, raw["sequenceNumber"].as_u64().unwrap());
        let mut tampered = raw.clone();
        tampered["message"]["delayedMessagesRead"] =
            json!(raw["message"]["delayedMessagesRead"].as_u64().unwrap() + 1);
        assert!(feed_message(&tampered).is_err());
    }
}
