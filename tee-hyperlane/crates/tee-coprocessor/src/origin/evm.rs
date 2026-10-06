//! JSON-RPC helpers for EVM chains: blocks, logs and proofs.

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

    /// A read near the head: the free endpoint first, then `url` if it refuses.
    pub async fn recent(&self, method: &str, params: Value) -> Result<Value> {
        if self.logs_url != self.url {
            match self.call_at(&self.logs_url, method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => debug!(method, error = %e, "free endpoint refused; using the main one"),
            }
        }
        self.call(method, params).await
    }

    async fn call_at(&self, url: &str, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut request = self.http.post(url).json(&body);
        if method == "eth_getProof" {
            // A full node rebuilds an older block's trie by unwinding from the head, which took
            // 146 seconds for 1352 blocks on Sepolia. Every other call keeps the client's 60.
            request = request.timeout(std::time::Duration::from_secs(300));
        }
        let reply = request.send().await?;
        let status = reply.status();
        let text = reply.text().await?;
        let response: Value = serde_json::from_str(&text).map_err(|_| {
            let host = url.split('/').nth(2).unwrap_or(url);
            anyhow::anyhow!(
                "{method}: {host} answered HTTP {status} with {}",
                crate::brief(&text)
            )
        })?;
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
            .recent("eth_getBlockByHash", json!([block_hash, false]))
            .await?;
        anyhow::ensure!(!block.is_null(), "no block {block_hash}");
        let header: alloy_consensus::Header = serde_json::from_value(block.clone())?;
        let mut rlp = Vec::new();
        header.encode(&mut rlp);
        if keccak256(&rlp) != block_hash {
            // Amsterdam appended fields this alloy does not know; encode from the JSON.
            rlp = encode_header(&block)?;
        }
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
        let r = self
            .call("eth_getProof", tree_keys(hook, base_slot, block))
            .await
            .context("eth_getProof on the merkle tree hook; the block may be outside this node's proof window")?;
        tree_proof(&r, hook)
    }

    /// As `tree_proof`, for a block near the head: from the free endpoint when it serves it.
    pub async fn recent_tree_proof(
        &self,
        hook: Address,
        base_slot: u64,
        block: u64,
    ) -> Result<Value> {
        let r = self
            .recent(
                "eth_getProof",
                tree_keys(hook, base_slot, json!(hex_number(block))),
            )
            .await
            .context("eth_getProof on the merkle tree hook near the head")?;
        tree_proof(&r, hook)
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
        for log in self
            .logs(mailbox, json!([DISPATCH_TOPIC]), from, to)
            .await?
        {
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
        for log in self.logs(hook, json!([INSERTED_TOPIC]), from, to).await? {
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

    /// A read from the log endpoint, which is the free one where a chain has two: the tracker
    /// reads only through this, so watching a chain never spends metered calls.
    pub async fn read(&self, method: &str, params: Value) -> Result<Value> {
        self.call_at(&self.logs_url, method, params).await
    }

    /// Every `Dispatch` from `mailbox` to one of `recipients` over `from..=to`, as the raw logs
    /// with the message bytes decoded: `(log, message)`.
    pub async fn dispatches_to(
        &self,
        mailbox: Address,
        recipients: &[[u8; 32]],
        from: u64,
        to: u64,
    ) -> Result<Vec<(Value, Vec<u8>)>> {
        let wanted: Vec<String> = recipients
            .iter()
            .map(|r| format!("0x{}", hex::encode(r)))
            .collect();
        let topics = json!([DISPATCH_TOPIC, null, null, wanted]);
        let mut out = Vec::new();
        for log in self.logs(mailbox, topics, from, to).await? {
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
            out.push((log, bytes));
        }
        Ok(out)
    }

    /// The newest log from `address` matching `topics` at or below block `from`, scanning
    /// backwards in windows as wide as the endpoint accepts, narrowed on refusal. No bound: it
    /// stops at the first match or at genesis, so it holds for any interval on any network.
    pub async fn last_log(
        &self,
        address: Address,
        topics: Value,
        from: u64,
    ) -> Result<Option<Value>> {
        let (mut window, mut end) = (MAX_LOG_WINDOW, from);
        loop {
            let start = end.saturating_sub(window - 1);
            let filter = json!([{ "address": address, "topics": topics, "fromBlock": hex_number(start), "toBlock": hex_number(end) }]);
            match self.call_at(&self.logs_url, "eth_getLogs", filter).await {
                Ok(result) => {
                    let found = result
                        .as_array()
                        .context("eth_getLogs did not return an array")?;
                    if let Some(log) = found.last() {
                        return Ok(Some(log.clone()));
                    }
                    if start == 0 {
                        return Ok(None);
                    }
                    end = start - 1;
                }
                Err(e) => {
                    anyhow::ensure!(
                        window > MIN_LOG_WINDOW,
                        "eth_getLogs refuses even {MIN_LOG_WINDOW} blocks at {start}: {e}"
                    );
                    window = (window / 4).max(MIN_LOG_WINDOW);
                    debug!(window, block = start, "narrowing the log window");
                }
            }
        }
    }

    /// `eth_getLogs` over any range, narrowing the window when the endpoint refuses. A pruned
    /// endpoint answers an old range with an empty array rather than an error; the route's
    /// leaf-count check is what catches that.
    async fn logs(
        &self,
        address: Address,
        topics: Value,
        from: u64,
        to: u64,
    ) -> Result<Vec<Value>> {
        let (mut window, mut cursor, mut out) = (MAX_LOG_WINDOW, from, Vec::new());
        while cursor <= to {
            let end = (cursor + window - 1).min(to);
            let filter = json!([{ "address": address, "topics": topics, "fromBlock": hex_number(cursor), "toBlock": hex_number(end) }]);
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

/// `eth_getProof` parameters for a hook's 33 tree slots at `block`.
fn tree_keys(hook: Address, base_slot: u64, block: Value) -> Value {
    let keys: Vec<String> = hyperlane_types::MerkleTreeSlots::new(base_slot)
        .storage_keys()
        .iter()
        .map(|k| format!("0x{}", hex::encode(k)))
        .collect();
    json!([hook, keys, block])
}

/// An `eth_getProof` answer for the tree slots, shaped as the enclave's `evm::TreeProof`.
fn tree_proof(r: &Value, hook: Address) -> Result<Value> {
    Ok(json!({
        "merkle_tree_hook": hook,
        "account": account(r),
        "account_proof": r["accountProof"],
        "storage_proof": r["storageProof"].as_array().context("storageProof")?.iter()
            .map(|s| json!({ "slot": s["key"], "value": s["value"], "proof": s["proof"] }))
            .collect::<Vec<_>>(),
    }))
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

/// A header's RLP from its JSON, in the order the forks appended fields, up to Amsterdam's
/// block access list hash and slot number. Every field the block has is encoded and none after
/// the first it lacks; the caller checks the result against the block hash.
pub fn encode_header(block: &Value) -> Result<Vec<u8>> {
    use alloy_rlp::Encodable;
    const FIELDS: [(&str, bool); 23] = [
        ("parentHash", false),
        ("sha3Uncles", false),
        ("miner", false),
        ("stateRoot", false),
        ("transactionsRoot", false),
        ("receiptsRoot", false),
        ("logsBloom", false),
        ("difficulty", true),
        ("number", true),
        ("gasLimit", true),
        ("gasUsed", true),
        ("timestamp", true),
        ("extraData", false),
        ("mixHash", false),
        ("nonce", false),
        ("baseFeePerGas", true),
        ("withdrawalsRoot", false),
        ("blobGasUsed", true),
        ("excessBlobGas", true),
        ("parentBeaconBlockRoot", false),
        ("requestsHash", false),
        ("blockAccessListHash", false),
        ("slotNumber", true),
    ];
    let mut items: Vec<u8> = Vec::new();
    for (name, number) in FIELDS {
        let Some(text) = block[name].as_str() else {
            break;
        };
        if number {
            alloy_primitives::U256::from_str_radix(text.trim_start_matches("0x"), 16)
                .with_context(|| name.to_string())?
                .encode(&mut items);
        } else {
            hex::decode(text.trim_start_matches("0x"))
                .with_context(|| name.to_string())?
                .as_slice()
                .encode(&mut items);
        }
    }
    let mut out = Vec::new();
    alloy_rlp::Header {
        list: true,
        payload_length: items.len(),
    }
    .encode(&mut out);
    out.extend_from_slice(&items);
    Ok(out)
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

#[cfg(test)]
mod header_tests {
    use super::*;

    /// Sepolia's last Prague block and an Amsterdam one: both re-encode to their own hash.
    #[test]
    fn a_header_encodes_to_its_hash_on_both_sides_of_amsterdam() {
        for name in ["sepolia_block_prague.json", "sepolia_block_amsterdam.json"] {
            let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
            let block: Value =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            let hash: B256 = block["hash"].as_str().unwrap().parse().unwrap();
            assert_eq!(keccak256(encode_header(&block).unwrap()), hash, "{name}");
            let decoded =
                tee_node::evm::BlockHeader::decode(&encode_header(&block).unwrap()).unwrap();
            assert_eq!(
                decoded.number,
                quantity(&block["number"]).unwrap(),
                "{name}"
            );
        }
    }
}
