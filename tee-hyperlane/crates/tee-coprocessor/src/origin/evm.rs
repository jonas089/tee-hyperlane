//! Reading an EVM chain over JSON-RPC: blocks, logs, and proofs of the Hyperlane tree.
//!
//! Shared by every EVM origin. Nothing read here is trusted; the enclave re-proves all of it.

use alloy_primitives::{keccak256, Address, B256};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use tracing::debug;

use crate::origin::Message;

/// Largest `eth_getLogs` range asked for at once; narrowed on refusal down to `MIN_LOG_WINDOW`.
const MAX_LOG_WINDOW: u64 = 10_000;
const MIN_LOG_WINDOW: u64 = 10;
/// `Mailbox.Dispatch(address,uint32,bytes32,bytes)` and `MerkleTreeHook.InsertedIntoTree`.
const DISPATCH_TOPIC: &str = "0x769f711d20c679153d382254f59892613b58a97cc876b249134ac25c80f9c814";
const INSERTED_TOPIC: &str = "0x253a3a04cab70d47c1504809242d9350cd81627b4f1d50753e159cf8cd76ed33";

pub struct Rpc {
    url: String,
    /// Where `eth_getLogs` goes. Usually `url`, but archive endpoints on free plans cap the log
    /// range at ten blocks, so an L2 may read logs from a second endpoint.
    logs_url: String,
    http: reqwest::Client,
}

impl Rpc {
    pub fn new(url: &str, logs_url: Option<&str>) -> Self {
        Self {
            url: url.to_string(),
            logs_url: logs_url.unwrap_or(url).to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("http client"),
        }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_at(&self.url, method, params).await
    }

    async fn call_at(&self, url: &str, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let response: Value = self.http.post(url).json(&body).send().await?.json().await?;
        if let Some(err) = response.get("error") {
            anyhow::bail!("{method}: {err}");
        }
        Ok(response["result"].clone())
    }

    pub async fn block_number(&self) -> Result<u64> {
        quantity(&self.call("eth_blockNumber", json!([])).await?)
    }

    pub async fn block(&self, number: u64) -> Result<Value> {
        let block = self
            .call("eth_getBlockByNumber", json!([hex_number(number), false]))
            .await?;
        anyhow::ensure!(!block.is_null(), "no block {number}");
        Ok(block)
    }

    pub async fn state_root(&self, number: u64) -> Result<B256> {
        Ok(self.block(number).await?["stateRoot"]
            .as_str()
            .context("stateRoot")?
            .parse()?)
    }

    /// A block's header, RLP-encoded, checked to hash to the block's own hash so a changed
    /// header layout fails here rather than in the enclave.
    pub async fn header_rlp(&self, block_hash: B256) -> Result<(Vec<u8>, Value)> {
        use alloy_rlp::Encodable;
        let block = self
            .call("eth_getBlockByHash", json!([block_hash, false]))
            .await?;
        anyhow::ensure!(!block.is_null(), "no block {block_hash}");
        let header: alloy_consensus::Header = serde_json::from_value(block.clone())?;
        let mut rlp = Vec::new();
        header.encode(&mut rlp);
        anyhow::ensure!(
            keccak256(&rlp) == block_hash,
            "re-encoded header of {block_hash} hashes differently"
        );
        Ok((rlp, block))
    }

    /// `eth_getProof` for a `MerkleTreeHook`'s 33 tree slots, shaped as the enclave's
    /// `evm::TreeProof`. `block` is a number, or `"latest"` for an endpoint that serves nothing
    /// older (Eden's).
    pub async fn tree_proof(&self, hook: Address, base_slot: u64, block: Value) -> Result<Value> {
        let keys: Vec<String> = hyperlane_types::MerkleTreeSlots::new(base_slot)
            .storage_keys()
            .iter()
            .map(|k| format!("0x{}", hex::encode(k)))
            .collect();
        let r = self
            .call("eth_getProof", json!([hook, keys, block]))
            .await
            .context("eth_getProof on the merkle tree hook; the block may be outside this node's proof window")?;
        Ok(json!({
            "merkle_tree_hook": hook,
            "account": account(&r),
            "account_proof": r["accountProof"],
            "storage_proof": r["storageProof"].as_array().context("storageProof")?.iter()
                .map(|s| json!({ "slot": s["key"], "value": s["value"], "proof": s["proof"] }))
                .collect::<Vec<_>>(),
        }))
    }

    /// Every message the hook took in over `from..=to`, in tree order.
    ///
    /// Read as two log scans joined on message id: the mailbox's `Dispatch` carries the bytes,
    /// the hook's `InsertedIntoTree` says which tree and at which index. Scanning the hook is
    /// what keeps a second hook on the same mailbox from being counted as ours.
    pub async fn dispatched(
        &self,
        mailbox: Address,
        hook: Address,
        from: u64,
        to: u64,
    ) -> Result<Vec<Message>> {
        let mut bodies = Vec::new();
        for log in self.logs(mailbox, DISPATCH_TOPIC, from, to).await? {
            let data = hex::decode(
                log["data"]
                    .as_str()
                    .context("data")?
                    .trim_start_matches("0x"),
            )?;
            anyhow::ensure!(data.len() >= 64, "dispatch log too short");
            let len = u64::from_be_bytes(data[56..64].try_into()?) as usize;
            let bytes = data
                .get(64..64 + len)
                .context("dispatch log shorter than its message")?
                .to_vec();
            bodies.push((keccak256(&bytes).0, bytes));
        }
        let mut out = Vec::new();
        for log in self.logs(hook, INSERTED_TOPIC, from, to).await? {
            let data = hex::decode(
                log["data"]
                    .as_str()
                    .context("data")?
                    .trim_start_matches("0x"),
            )?;
            anyhow::ensure!(data.len() >= 64, "insert log too short");
            let id: [u8; 32] = data[..32].try_into()?;
            let index = u32::from_be_bytes(data[60..64].try_into()?);
            let bytes = bodies
                .iter()
                .find(|(h, _)| *h == id)
                .map(|(_, b)| b.clone())
                .unwrap_or_default();
            out.push((index, Message { id, bytes }));
        }
        out.sort_by_key(|(index, _)| *index);
        Ok(out.into_iter().map(|(_, m)| m).collect())
    }

    /// `eth_getLogs` over any range, narrowing the window when the endpoint refuses. A pruned
    /// endpoint answers an old range with an empty array rather than an error; the route's
    /// leaf-count check is what catches that.
    async fn logs(&self, address: Address, topic: &str, from: u64, to: u64) -> Result<Vec<Value>> {
        let (mut window, mut cursor, mut out) = (MAX_LOG_WINDOW, from, Vec::new());
        while cursor <= to {
            let end = (cursor + window - 1).min(to);
            let filter = json!([{ "address": address, "topics": [topic], "fromBlock": hex_number(cursor), "toBlock": hex_number(end) }]);
            match self.call_at(&self.logs_url, "eth_getLogs", filter).await {
                Ok(result) => {
                    out.extend(
                        result
                            .as_array()
                            .context("eth_getLogs did not return an array")?
                            .clone(),
                    );
                    cursor = end + 1;
                }
                Err(e) => {
                    anyhow::ensure!(
                        window > MIN_LOG_WINDOW,
                        "eth_getLogs refuses even {MIN_LOG_WINDOW} blocks at {cursor}: {e}"
                    );
                    window = (window / 4).max(MIN_LOG_WINDOW);
                    debug!(window, block = cursor, "narrowing the log window");
                }
            }
        }
        Ok(out)
    }
}

/// The account half of an `eth_getProof` answer, shaped as the enclave's `ClaimedAccount`.
pub fn account(proof: &Value) -> Value {
    json!({
        "nonce": proof["nonce"],
        "balance": proof["balance"],
        "storage_root": proof["storageHash"],
        "code_hash": proof["codeHash"],
    })
}

pub fn hex_number(n: u64) -> String {
    format!("0x{n:x}")
}

pub fn quantity(v: &Value) -> Result<u64> {
    Ok(u64::from_str_radix(
        v.as_str()
            .context("expected a hex quantity")?
            .trim_start_matches("0x"),
        16,
    )?)
}
