//! Ethereum (Sepolia) as an origin: finding the light-client updates and tree proofs the
//! enclave's `ethereum::Ethereum` verifies.
//!
//! The light-client store is never kept. It is rebuilt each tick from the ISM's own trusted
//! state, which is what lets a route resume after any outage with no memory of its own. Arbitrum
//! and Base reuse this whole half: their ISMs commit to an Ethereum store too.

pub mod arbitrum;
pub mod base;
pub mod l2_shared;

use alloy_primitives::{Address, B256};
use anyhow::{Context, Result};
use async_trait::async_trait;
use helios_consensus_core::types::{
    Bootstrap, FinalityUpdate, Fork, Forks, LightClientStore, Update,
};
use helios_consensus_core::{apply_bootstrap, verify_bootstrap};
use serde::Deserialize;
use serde_json::{json, Value};
use tee_node::ethereum::{EthereumStore, Spec, ETHEREUM, TREE_SLOT};
use tee_node::state::IsmState;
use tracing::{debug, info};
use tree_hash::TreeHash;

use crate::origin::evm::Rpc;
use crate::origin::{self, Cache, Indexer, Message, Step};

const SECONDS_PER_SLOT: u64 = 12;
const SLOTS_PER_EPOCH: u64 = 32;
/// A sync-committee period: 256 epochs, about 27 hours on Sepolia.
const SLOTS_PER_SYNC_PERIOD: u64 = 256 * SLOTS_PER_EPOCH;
/// How far back to search for a checkpoint that rebuilds the ISM's store, when no hint does.
/// A recovery path, not the normal one: a healthy route finds its store from a hint in one
/// request. 512 epochs is a bit over two days, which is how stale a stuck route may be.
const MAX_CHECKPOINT_SEARCH_EPOCHS: u64 = 512;

/// `[chains.<name>]` for an Ethereum chain.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub domain: u32,
    /// Execution RPC. Serves state proofs only for recent blocks.
    pub rpc: String,
    /// For reads at the ISM's trusted height, which is older than a public node keeps proofs
    /// for once a route has been down a few hours.
    pub archive_rpc: Option<String>,
    /// Where transactions and ISM reads go when this chain is a destination, if not `rpc`.
    /// Proof reads want an archive; relaying wants a node that is not rate-limited or metered.
    pub send_rpc: Option<String>,
    /// Beacon API, needed when this chain is an origin.
    pub beacon_rpc: Option<String>,
    pub mailbox: Address,
    pub merkle_tree_hook: Address,
    /// A checkpoint to try first when rebuilding the store, such as the one the ISM was
    /// created from. A hint only: used if it rebuilds the ISM's store, skipped otherwise.
    pub checkpoint: Option<String>,
}

pub struct Ethereum {
    beacon: Beacon,
    rpc: Rpc,
    history: Rpc,
    mailbox: Address,
    hook: Address,
    checkpoint: Option<String>,
    cache: Cache,
}

/// Ethereum's half of a step, which Arbitrum and Base build on.
pub struct L1Step {
    /// The enclave's `ethereum::Input`: the store the ISM committed to, and the updates.
    pub input: Value,
    /// The finalized execution block the updates land on.
    pub block: u64,
    pub state_root: B256,
}

impl Ethereum {
    pub fn new(config: Config, cache: Cache) -> Result<Self> {
        let beacon = config
            .beacon_rpc
            .context("an Ethereum origin needs `beacon_rpc`")?;
        Ok(Self {
            beacon: Beacon::new(&beacon),
            history: Rpc::new(config.archive_rpc.as_deref().unwrap_or(&config.rpc), None),
            rpc: Rpc::new(&config.rpc, None),
            mailbox: config.mailbox,
            hook: config.merkle_tree_hook,
            checkpoint: config.checkpoint,
            cache,
        })
    }

    /// The L1 archive endpoint, for the L2s' proofs at the finalized block.
    pub fn history(&self) -> &Rpc {
        &self.history
    }

    /// Rebuild the ISM's store and walk it to the current finalized head. `None` when that head
    /// cannot be committed to yet (see the mid-epoch check below).
    pub async fn l1_step(&self, trusted: &IsmState) -> Result<Option<L1Step>> {
        let (store, used) = self.rebuild_store(trusted).await?;
        let finality = self.beacon.finality_update().await?;

        // A light-client bootstrap exists only for a checkpoint on an epoch boundary. When that
        // slot is empty the finalized checkpoint keeps the root of the block before it, which no
        // beacon node will bootstrap from, so a store committed there could never be rebuilt and
        // the route would stop for good. About seven percent of boundaries; waiting costs one
        // epoch, committing costs the route.
        let slot = finality.finalized_header().beacon().slot;
        if slot % SLOTS_PER_EPOCH != 0 {
            debug!(
                slot,
                "finalized checkpoint is mid-epoch; waiting for the next epoch"
            );
            self.remember(&[&used]);
            return Ok(None);
        }

        // Committee updates bridge any sync-committee period boundary since the store's last
        // update. Without them the finality update is refused once a period has passed, which
        // wedged every Ethereum-backed route roughly daily before this was carried.
        let from = store.store.finalized_header.beacon().slot / SLOTS_PER_SYNC_PERIOD;
        let to = slot / SLOTS_PER_SYNC_PERIOD;
        let committee_updates = if to > from {
            debug!(periods = to - from, "carrying sync committee updates");
            self.beacon.updates(from, to - from).await?
        } else {
            Vec::new()
        };

        // The store the ISM moves to is the one this finality update leaves behind, and its
        // checkpoint is that header's root. Remembered now, ahead of the one just used, so the
        // next tick finds it in one request whether or not this step lands.
        let next = format!(
            "0x{}",
            hex::encode(finality.finalized_header().beacon().tree_hash_root())
        );
        self.remember(&[&next, &used]);

        let execution = finality
            .finalized_header()
            .execution()
            .map_err(|_| anyhow::anyhow!("finalized header has no execution payload"))?;
        Ok(Some(L1Step {
            block: *execution.block_number(),
            state_root: *execution.state_root(),
            input: json!({
                "store": store,
                "updates": { "committee_updates": committee_updates, "finality_update": finality },
            }),
        }))
    }

    fn remember(&self, checkpoints: &[&str]) {
        let mut all: Vec<String> = checkpoints.iter().map(|c| c.to_string()).collect();
        for old in self.cache.read("checkpoints").unwrap_or_default().lines() {
            if !all.iter().any(|c| c == old) {
                all.push(old.to_string());
            }
        }
        all.truncate(3);
        self.cache.write("checkpoints", &all.join("\n"));
    }

    /// Rebuild the exact store the ISM committed to, and say which checkpoint did it.
    ///
    /// Hints first, cheapest to most expensive: the checkpoints this route remembered, the
    /// configured one, then for an Ethereum-origin ISM the checkpoint its timestamp names
    /// (post-merge, `timestamp = genesis + slot * 12`). Only then a walk back through finalized
    /// checkpoints. Every candidate is checked against the commitment, so a wrong hint costs a
    /// request and nothing else.
    async fn rebuild_store(&self, trusted: &IsmState) -> Result<(EthereumStore, String)> {
        let mut hints: Vec<String> = self
            .cache
            .read("checkpoints")
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect();
        hints.extend(self.checkpoint.clone());
        if trusted.origin_domain == ETHEREUM.domain {
            let genesis = self.beacon.genesis().await?.0;
            let slot = trusted.timestamp.saturating_sub(genesis) / SECONDS_PER_SLOT;
            if let Ok(root) = self.beacon.block_root(slot).await {
                hints.push(root);
            }
        }
        for checkpoint in &hints {
            if let Ok(store) = self.store_at(checkpoint).await {
                if store.commitment() == trusted.lc_store_commit {
                    return Ok((store, checkpoint.clone()));
                }
            }
            debug!(checkpoint, "hint does not rebuild this ISM's store");
        }

        let head = self.beacon.finalized_slot().await?;
        for epoch in 0..MAX_CHECKPOINT_SEARCH_EPOCHS {
            let Ok(checkpoint) = self
                .beacon
                .block_root(head.saturating_sub(epoch * SLOTS_PER_EPOCH))
                .await
            else {
                continue;
            };
            if let Ok(store) = self.store_at(&checkpoint).await {
                if store.commitment() == trusted.lc_store_commit {
                    return Ok((store, checkpoint));
                }
            }
        }
        anyhow::bail!(
            "no finalized checkpoint in the last {MAX_CHECKPOINT_SEARCH_EPOCHS} epochs rebuilds this ISM's \
             light-client store; set `checkpoint` on the chain"
        )
    }

    /// A store bootstrapped from `checkpoint`, after checking the bootstrap against it.
    pub async fn store_at(&self, checkpoint: &str) -> Result<EthereumStore> {
        let (genesis_time, genesis_root, forks) = self.beacon.genesis().await?;
        let bootstrap = self.beacon.bootstrap(checkpoint).await?;
        verify_bootstrap::<Spec>(&bootstrap, checkpoint.parse()?, &forks).map_err(|e| {
            anyhow::anyhow!("bootstrap does not match checkpoint {checkpoint}: {e}")
        })?;
        let mut store = LightClientStore::default();
        apply_bootstrap::<Spec>(&mut store, &bootstrap);
        Ok(EthereumStore {
            store,
            genesis_root,
            genesis_time,
            forks,
        })
    }

    /// The store a new ISM starts from: the configured checkpoint, or the finalized one.
    pub async fn genesis_store(&self) -> Result<EthereumStore> {
        let checkpoint = match &self.checkpoint {
            Some(c) => c.clone(),
            None => self.beacon.finalized_root().await?,
        };
        info!(checkpoint, "anchoring to checkpoint");
        self.store_at(&checkpoint).await
    }
}

#[async_trait]
impl Indexer for Ethereum {
    async fn gather(&self, trusted: &IsmState) -> Result<Step> {
        let Some(l1) = self.l1_step(trusted).await? else {
            return Ok(Step::idle(trusted.height));
        };
        if l1.block <= trusted.height {
            return Ok(Step::idle(l1.block));
        }
        let tree = self
            .rpc
            .tree_proof(
                self.hook,
                TREE_SLOT,
                json!(crate::origin::evm::hex_number(l1.block)),
            )
            .await?;
        let snapshot = self
            .history
            .tree_proof(
                self.hook,
                TREE_SLOT,
                json!(crate::origin::evm::hex_number(trusted.height)),
            )
            .await
            .context("reading the tree at the trusted height; set `archive_rpc` if pruned")?;
        Ok(Step {
            head: l1.block,
            leaves: origin::leaves(
                &ETHEREUM,
                &snapshot,
                trusted.state_root,
                &tree,
                l1.state_root,
            )?,
            chain: ETHEREUM.name,
            input: l1.input,
            tree,
            tree_snapshot: snapshot,
            tree_address: tee_node::evm::padded(self.hook),
        })
    }

    async fn index(&self, from: u64, to: u64) -> Result<Vec<Message>> {
        self.history
            .dispatched(self.mailbox, self.hook, from + 1, to)
            .await
    }

    async fn bootstrap(&self, identity: [u8; 32], _height: Option<u64>) -> Result<IsmState> {
        let store = self.genesis_store().await?;
        let root = store.root()?;
        Ok(IsmState {
            state_root: root.state_root.0,
            origin_domain: ETHEREUM.domain,
            height: root.height,
            timestamp: root.timestamp,
            lc_store_commit: store.commitment(),
            identity_digest: identity,
        })
    }
}

/// The beacon API. A data source only: every signature it serves is re-checked in the enclave.
struct Beacon {
    url: String,
    http: reqwest::Client,
    /// Genesis and fork schedule never change, and the store rebuild asks for them per candidate.
    genesis: tokio::sync::OnceCell<(u64, B256, Forks)>,
}

/// Beacon responses wrap their payload with the fork it was produced under.
#[derive(Deserialize)]
struct Versioned<T> {
    data: T,
}

impl Beacon {
    fn new(url: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("http client"),
            genesis: tokio::sync::OnceCell::new(),
        }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.url);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| url.clone())?;
        anyhow::ensure!(
            response.status().is_success(),
            "{url} -> {}",
            response.status()
        );
        Ok(response
            .json::<Versioned<T>>()
            .await
            .with_context(|| format!("decoding {url}"))?
            .data)
    }

    async fn finalized_root(&self) -> Result<String> {
        Ok(self
            .get::<Value>("/eth/v1/beacon/headers/finalized")
            .await?["root"]
            .as_str()
            .context("root")?
            .to_string())
    }

    async fn finalized_slot(&self) -> Result<u64> {
        let header: Value = self.get("/eth/v1/beacon/headers/finalized").await?;
        Ok(header["header"]["message"]["slot"]
            .as_str()
            .context("slot")?
            .parse()?)
    }

    async fn block_root(&self, slot: u64) -> Result<String> {
        Ok(self
            .get::<Value>(&format!("/eth/v1/beacon/blocks/{slot}/root"))
            .await?["root"]
            .as_str()
            .context("root")?
            .to_string())
    }

    async fn bootstrap(&self, checkpoint: &str) -> Result<Bootstrap<Spec>> {
        self.get(&format!(
            "/eth/v1/beacon/light_client/bootstrap/{checkpoint}"
        ))
        .await
    }

    async fn finality_update(&self) -> Result<FinalityUpdate<Spec>> {
        self.get("/eth/v1/beacon/light_client/finality_update")
            .await
    }

    /// Sync-committee updates from `period` on. This endpoint returns a bare array of wrapped
    /// updates rather than one wrapped value.
    async fn updates(&self, period: u64, count: u64) -> Result<Vec<Update<Spec>>> {
        let url = format!(
            "{}/eth/v1/beacon/light_client/updates?start_period={period}&count={count}",
            self.url
        );
        let raw: Vec<Versioned<Update<Spec>>> = self.http.get(&url).send().await?.json().await?;
        Ok(raw.into_iter().map(|v| v.data).collect())
    }

    /// Genesis time, genesis validators root, and the fork schedule.
    async fn genesis(&self) -> Result<(u64, B256, Forks)> {
        self.genesis
            .get_or_try_init(|| self.read_genesis())
            .await
            .cloned()
    }

    async fn read_genesis(&self) -> Result<(u64, B256, Forks)> {
        let genesis: Value = self.get("/eth/v1/beacon/genesis").await?;
        let spec: Value = self.get("/eth/v1/config/spec").await?;
        let fork = |name: &str| -> Result<Fork> {
            Ok(Fork {
                epoch: spec[format!("{name}_FORK_EPOCH")]
                    .as_str()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0),
                fork_version: spec[format!("{name}_FORK_VERSION")]
                    .as_str()
                    .with_context(|| format!("{name}_FORK_VERSION missing"))?
                    .parse()?,
            })
        };
        Ok((
            genesis["genesis_time"]
                .as_str()
                .context("genesis_time")?
                .parse()?,
            genesis["genesis_validators_root"]
                .as_str()
                .context("genesis_validators_root")?
                .parse()?,
            Forks {
                genesis: fork("GENESIS")?,
                altair: fork("ALTAIR")?,
                bellatrix: fork("BELLATRIX")?,
                capella: fork("CAPELLA")?,
                deneb: fork("DENEB")?,
                electra: fork("ELECTRA")?,
                fulu: fork("FULU")?,
            },
        ))
    }
}
