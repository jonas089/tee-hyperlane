//! Reading Ethereum, so the enclave can verify it.
//!
//! The beacon API is a data source and nothing more: every sync-committee signature is
//! re-checked inside the enclave against the store the ISM already commits to, and every
//! storage value is re-proven against the execution state root that verification yields.
//! A lying endpoint produces a rejected attestation, never a wrong root.

use anyhow::{Context, Result};
use helios_consensus_core::consensus_spec::MainnetConsensusSpec;
use helios_consensus_core::types::{Bootstrap, FinalityUpdate, Fork, Forks, Update};
use serde::Deserialize;

pub type Spec = MainnetConsensusSpec;

/// Beacon API responses wrap their payload and tag it with the fork it was produced under.
#[derive(Deserialize)]
struct Versioned<T> {
    data: T,
}

pub struct EthereumReader {
    beacon: String,
    http: reqwest::Client,
}

impl EthereumReader {
    pub fn new(beacon_url: &str) -> Self {
        Self {
            beacon: beacon_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.beacon);
        let response = self.http.get(&url).send().await.with_context(|| url.clone())?;
        anyhow::ensure!(response.status().is_success(), "{url} -> {}", response.status());
        Ok(response.json().await.with_context(|| format!("decoding {url}"))?)
    }

    /// The block root a light client is anchored to.
    ///
    /// This is the weak-subjectivity checkpoint: whoever creates the ISM chooses it, and
    /// everyone can see which one they chose, because it ends up in the ISM's genesis state.
    pub async fn finalized_root(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct Header {
            root: String,
        }
        let v: Versioned<Header> = self.get("/eth/v1/beacon/headers/finalized").await?;
        Ok(v.data.root)
    }

    /// The block root of the beacon block at `slot`.
    pub async fn block_root_at_slot(&self, slot: u64) -> Result<String> {
        #[derive(Deserialize)]
        struct Root {
            root: String,
        }
        let v: Versioned<Root> = self.get(&format!("/eth/v1/beacon/blocks/{slot}/root")).await?;
        Ok(v.data.root)
    }

    /// Slot of the current finalized header.
    pub async fn finalized_slot(&self) -> Result<u64> {
        #[derive(Deserialize)]
        struct Header {
            header: Message,
        }
        #[derive(Deserialize)]
        struct Message {
            message: Slot,
        }
        #[derive(Deserialize)]
        struct Slot {
            slot: String,
        }
        let v: Versioned<Header> = self.get("/eth/v1/beacon/headers/finalized").await?;
        Ok(v.data.header.message.slot.parse()?)
    }

    pub async fn bootstrap(&self, checkpoint: &str) -> Result<Bootstrap<Spec>> {
        let v: Versioned<Bootstrap<Spec>> = self
            .get(&format!("/eth/v1/beacon/light_client/bootstrap/{checkpoint}"))
            .await?;
        Ok(v.data)
    }

    /// Sync-committee updates from `period` onwards, in order.
    pub async fn updates(&self, period: u64, count: u64) -> Result<Vec<Update<Spec>>> {
        let raw: Vec<Versioned<Update<Spec>>> = self
            .get(&format!(
                "/eth/v1/beacon/light_client/updates?start_period={period}&count={count}"
            ))
            .await?;
        Ok(raw.into_iter().map(|v| v.data).collect())
    }

    pub async fn finality_update(&self) -> Result<FinalityUpdate<Spec>> {
        let v: Versioned<FinalityUpdate<Spec>> =
            self.get("/eth/v1/beacon/light_client/finality_update").await?;
        Ok(v.data)
    }

    /// Chain identity and fork schedule, both of which the signature checks depend on.
    pub async fn chain_config(&self) -> Result<ChainConfig> {
        #[derive(Deserialize)]
        struct Genesis {
            genesis_time: String,
            genesis_validators_root: String,
        }
        let genesis: Versioned<Genesis> = self.get("/eth/v1/beacon/genesis").await?;
        let spec: Versioned<serde_json::Value> = self.get("/eth/v1/config/spec").await?;

        let fork = |name: &str| -> Result<Fork> {
            let version = spec.data[format!("{name}_FORK_VERSION")]
                .as_str()
                .with_context(|| format!("{name}_FORK_VERSION missing"))?;
            // Genesis has no epoch field; every later fork does.
            let epoch = spec.data[format!("{name}_FORK_EPOCH")]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            Ok(Fork { epoch, fork_version: version.parse()? })
        };

        Ok(ChainConfig {
            genesis_time: genesis.data.genesis_time.parse()?,
            genesis_root: genesis.data.genesis_validators_root.parse()?,
            forks: Forks {
                genesis: fork("GENESIS")?,
                altair: fork("ALTAIR")?,
                bellatrix: fork("BELLATRIX")?,
                capella: fork("CAPELLA")?,
                deneb: fork("DENEB")?,
                electra: fork("ELECTRA")?,
                fulu: fork("FULU")?,
            },
        })
    }
}

pub struct ChainConfig {
    pub genesis_time: u64,
    pub genesis_root: alloy_primitives::B256,
    pub forks: Forks,
}

/// Slots since genesis, which bounds how far ahead an update may claim to be.
/// Ethereum's slot time. Fixed since genesis, and what ties a payload timestamp to a slot.
pub const SECONDS_PER_SLOT: u64 = 12;

pub fn expected_current_slot(genesis_time: u64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(genesis_time);
    now.saturating_sub(genesis_time) / SECONDS_PER_SLOT
}

/// The execution-side reads: storage proofs and dispatch logs.
///
/// Both are untrusted. Storage is re-proven inside the enclave against the state root the
/// light client produced, and a log that names a message the tree does not contain simply
/// fails the replay.
pub struct ExecutionReader {
    rpc: String,
    http: reqwest::Client,
}

/// Hyperlane's `Dispatch(address,uint32,bytes32,bytes)`.
const DISPATCH_TOPIC: &str =
    "0x769f711d20c679153d382254f59892613b58a97cc876b249134ac25c80f9c814";
/// `MerkleTreeHook.InsertedIntoTree(bytes32,uint32)`.
const INSERTED_TOPIC: &str =
    "0x253a3a04cab70d47c1504809242d9350cd81627b4f1d50753e159cf8cd76ed33";

#[derive(Debug, Clone)]
pub struct EvmDispatch {
    pub block: u64,
    pub tree_index: u32,
    pub message_id: [u8; 32],
    pub message: Vec<u8>,
}

impl ExecutionReader {
    pub fn new(rpc: &str) -> Self {
        Self { rpc: rpc.to_string(), http: reqwest::Client::new() }
    }

    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params
        });
        let response: serde_json::Value =
            self.http.post(&self.rpc).json(&body).send().await?.json().await?;
        if let Some(err) = response.get("error") {
            anyhow::bail!("{method}: {err}");
        }
        Ok(response["result"].clone())
    }

    /// Prove the merkle tree hook's 33 storage slots at `block`.
    pub async fn merkle_tree_proof(
        &self,
        hook: alloy_primitives::Address,
        base_slot: u64,
        block: u64,
    ) -> Result<tee_node::hyperlane_state::EvmTreeProof> {
        let slots = hyperlane_types::MerkleTreeSlots::new(base_slot);
        let keys: Vec<String> = slots
            .storage_keys()
            .iter()
            .map(|k| format!("0x{}", hex::encode(k)))
            .collect();

        let result = self
            .call(
                "eth_getProof",
                serde_json::json!([hook, keys, format!("0x{block:x}")]),
            )
            .await
            .context("eth_getProof; the block may be outside this node's proof window")?;

        Ok(serde_json::from_value(serde_json::json!({
            "merkle_tree_hook": hook,
            "base_slot": base_slot,
            "account": {
                "nonce": result["nonce"],
                "balance": result["balance"],
                "storage_root": result["storageHash"],
                "code_hash": result["codeHash"],
            },
            "account_proof": result["accountProof"],
            "storage_proof": result["storageProof"]
                .as_array()
                .context("storageProof")?
                .iter()
                .map(|s| serde_json::json!({
                    "slot": s["key"], "value": s["value"], "proof": s["proof"]
                }))
                .collect::<Vec<_>>(),
        }))?)
    }

    /// Messages inserted into the tree between two blocks, in insert order.
    pub async fn dispatched_messages(
        &self,
        mailbox: alloy_primitives::Address,
        hook: alloy_primitives::Address,
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<EvmDispatch>> {
        let range = |address, topic| {
            serde_json::json!([{
                "address": address,
                "topics": [topic],
                "fromBlock": format!("0x{from_block:x}"),
                "toBlock": format!("0x{to_block:x}"),
            }])
        };

        let dispatches = self.call("eth_getLogs", range(mailbox, DISPATCH_TOPIC)).await?;
        let inserts = self.call("eth_getLogs", range(hook, INSERTED_TOPIC)).await?;

        // Dispatch carries the message as an ABI-encoded `bytes`: offset, length, payload.
        let mut messages: Vec<Vec<u8>> = Vec::new();
        for log in dispatches.as_array().context("dispatch logs")? {
            let data = hex::decode(log["data"].as_str().context("data")?.trim_start_matches("0x"))?;
            anyhow::ensure!(data.len() >= 64, "dispatch log too short");
            let len = u64::from_be_bytes(data[56..64].try_into()?) as usize;
            messages.push(data[64..64 + len].to_vec());
        }

        let mut out = Vec::new();
        for log in inserts.as_array().context("insert logs")? {
            let data = hex::decode(log["data"].as_str().context("data")?.trim_start_matches("0x"))?;
            anyhow::ensure!(data.len() >= 64, "insert log too short");
            let message_id: [u8; 32] = data[..32].try_into()?;
            let tree_index = u32::from_be_bytes(data[60..64].try_into()?);
            let block = u64::from_str_radix(
                log["blockNumber"].as_str().context("blockNumber")?.trim_start_matches("0x"),
                16,
            )?;
            let message = messages
                .iter()
                .find(|m| {
                    hyperlane_types::decode_hyperlane_message(m)
                        .map(|d| hyperlane_types::get_message_id(&d) == message_id)
                        .unwrap_or(false)
                })
                .cloned()
                .unwrap_or_default();
            out.push(EvmDispatch { block, tree_index, message_id, message });
        }
        out.sort_by_key(|d| d.tree_index);
        Ok(out)
    }
}
