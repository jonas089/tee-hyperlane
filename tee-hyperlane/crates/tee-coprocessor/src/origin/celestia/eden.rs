//! Eden as an origin: finding the Celestia block that carries Eden's signed header, the blocks
//! to re-execute up to it, and the tree proofs, all of which the enclave's
//! `celestia::eden::Eden` verifies.
//!
//! Three things make this origin harder to follow than the others, and each is handled here:
//!
//! * Eden's node serves `eth_getProof` only at `latest`. So a tree proof is captured every tick
//!   and kept, and a step can only attest a height a proof was captured for. The ISM's own
//!   trusted height is the one proof that must never be dropped: it is every step's snapshot.
//! * The ISM records Eden's height, not the Celestia height its light-client store sits at,
//!   so that height is remembered, and searched for when the hint is missing.
//! * Eden posts to Celestia in batches, so the newest Celestia block rarely carries a header;
//!   the DA node is walked back for one that does, remembering heights known to be empty.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use alloy_primitives::{Address, B256};
use anyhow::{Context, Result};
use async_trait::async_trait;
use celestia_types::namespace_data::NamespaceData;
use celestia_types::DataAvailabilityHeader;
use serde::Deserialize;
use serde_json::{json, Value};
use tee_node::celestia::eden::{Eden as EdenChain, SignedHeader, EDEN, TREE_SLOT};
use tee_node::celestia::CelestiaStore;
use tee_node::state::IsmState;
use tendermint_light_client_verifier::types::LightBlock;
use tracing::{debug, info, warn};

use super::Rpc as CelestiaRpc;
use crate::origin::evm::{hex_number, Rpc};
use crate::origin::{self, Cache, Indexer, Message, Step};

/// How far back to search for the Celestia header this ISM's store sits at, without a hint.
const MAX_STORE_SEARCH: u64 = 400;
/// Consecutive header failures after which the Celestia endpoint counts as down, not pruned.
const UNREACHABLE_AFTER: u32 = 5;
/// How far back to walk the DA node for a block carrying an Eden header.
const MAX_DA_WALK: u64 = 1200;
/// How many captured tree proofs to keep. Eden makes ten blocks a second.
const TREES_KEPT: usize = 400;
/// How far from `latest` to look for the block a `latest` proof was taken at.
const PROOF_HEIGHT_SEARCH: u64 = 60;
/// Bootstrap waits up to this many rounds of fifteen seconds for DA to publish the anchor.
const BOOTSTRAP_DA_TRIES: usize = 20;

/// Celestia heights whose Eden namespace is known to be empty. They stay empty, so there is no
/// reason to ask the DA node about them again on the next tick.
static EMPTY_DA_HEIGHTS: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(Default::default);

/// `[chains.<name>]` for Eden.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub domain: u32,
    /// The Celestia chain, by name, that Eden posts to.
    pub celestia: String,
    /// A celestia-node DA endpoint: the consensus RPC cannot serve rows or blobs.
    pub da_rpc: String,
    /// Eden's own RPC. It must serve `debug_executionWitness`, which re-execution needs.
    pub rpc: String,
    pub logs_rpc: Option<String>,
    /// Where transactions and ISM reads go when this chain is a destination, if not `rpc`.
    /// Proof reads want an archive; relaying wants a node that is not rate-limited or metered.
    pub send_rpc: Option<String>,
    pub mailbox: Address,
    pub merkle_tree_hook: Address,
    #[serde(default = "super::default_lag")]
    pub lag: u64,
}

pub struct Eden {
    celestia: CelestiaRpc,
    da: Da,
    rpc: Rpc,
    mailbox: Address,
    hook: Address,
    lag: u64,
    cache: Cache,
}

impl Eden {
    pub fn new(config: Config, celestia: super::Config, cache: Cache) -> Result<Self> {
        let _ = std::fs::create_dir_all(cache.path("trees"));
        Ok(Self {
            celestia: CelestiaRpc::new(&celestia.rpc)?,
            da: Da::new(&config.da_rpc),
            rpc: Rpc::new(&config.rpc, config.logs_rpc.as_deref()),
            mailbox: config.mailbox,
            hook: config.merkle_tree_hook,
            lag: config.lag,
            cache,
        })
    }

    /// The Celestia header the ISM's store sits at: the remembered heights first, then a walk
    /// back from `head`.
    async fn store_at(&self, commitment: [u8; 32], head: u64) -> Result<(u64, LightBlock)> {
        let remembered: Vec<u64> = self
            .cache
            .read("da-heights")
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.parse().ok())
            .collect();
        for h in remembered {
            if let Ok(block) = self.celestia.light_block(h).await {
                if (CelestiaStore {
                    trusted: block.clone(),
                })
                .commitment()
                    == commitment
                {
                    return Ok((h, block));
                }
            }
        }
        let mut failures = 0;
        for h in (head.saturating_sub(MAX_STORE_SEARCH)..=head).rev() {
            match self.celestia.light_block(h).await {
                Ok(block)
                    if (CelestiaStore {
                        trusted: block.clone(),
                    })
                    .commitment()
                        == commitment =>
                {
                    return Ok((h, block))
                }
                Ok(_) => failures = 0,
                Err(e) => {
                    failures += 1;
                    anyhow::ensure!(failures < UNREACHABLE_AFTER, "the celestia endpoint answered none of the last {UNREACHABLE_AFTER} header requests: {e}");
                }
            }
        }
        anyhow::bail!("no celestia header in the last {MAX_STORE_SEARCH} rebuilds this ISM's store; the route needs re-bootstrapping")
    }

    /// Capture a tree proof at Eden's latest block, and file it under the height it is for.
    /// `pinned` is the one height never pruned: the ISM's trusted height.
    async fn capture_tree(&self, pinned: u64) -> Result<u64> {
        let proof = self
            .rpc
            .tree_proof(self.hook, TREE_SLOT, json!("latest"))
            .await?;
        let account: tee_node::evm::ClaimedAccount =
            serde_json::from_value(proof["account"].clone())?;
        let proof_nodes: Vec<alloy_primitives::Bytes> =
            serde_json::from_value(proof["account_proof"].clone())?;
        // `latest` names no height, so find the block whose state root the proof verifies under.
        let head = self.rpc.block_number().await?;
        let mut height = None;
        for h in (head.saturating_sub(PROOF_HEIGHT_SEARCH)..=head + 2).rev() {
            let Ok(root) = self.rpc.state_root(h).await else {
                continue;
            };
            if tee_node::evm::verify_account_proof(root, self.hook, &account, &proof_nodes).is_ok()
            {
                height = Some(h);
                break;
            }
        }
        let height = height.with_context(|| format!("could not place the latest tree proof within {PROOF_HEIGHT_SEARCH} blocks of {head}"))?;
        self.cache
            .write(&format!("trees/{height}.json"), &proof.to_string());

        let mut kept = self.cached_heights();
        kept.retain(|h| *h != pinned);
        for old in &kept[..kept.len().saturating_sub(TREES_KEPT)] {
            let _ = std::fs::remove_file(self.cache.path(&format!("trees/{old}.json")));
        }
        Ok(height)
    }

    fn cached_tree(&self, height: u64) -> Option<Value> {
        serde_json::from_str(&self.cache.read(&format!("trees/{height}.json"))?).ok()
    }

    fn cached_heights(&self) -> Vec<u64> {
        let mut heights: Vec<u64> = std::fs::read_dir(self.cache.path("trees"))
            .into_iter()
            .flatten()
            .filter_map(|e| {
                e.ok()?
                    .file_name()
                    .to_str()?
                    .strip_suffix(".json")?
                    .parse()
                    .ok()
            })
            .collect();
        heights.sort_unstable();
        heights
    }

    /// Walk the DA node back from `from` for the newest block carrying an Eden header at one of
    /// `usable` heights, and posted no later than that block (a header claiming a future time is
    /// not evidence of anything).
    async fn find_post(
        &self,
        from: u64,
        usable: &[u64],
    ) -> Result<Option<(u64, NamespaceData, SignedHeader)>> {
        for h in (from.saturating_sub(MAX_DA_WALK)..=from).rev() {
            if EMPTY_DA_HEIGHTS.lock().is_ok_and(|seen| seen.contains(&h)) {
                continue;
            }
            let Ok(data) = self.da.namespace_data(h).await else {
                continue;
            };
            if data.rows().iter().all(|r| r.shares.is_empty()) {
                if let Ok(mut seen) = EMPTY_DA_HEIGHTS.lock() {
                    seen.insert(h);
                }
                continue;
            }
            let Ok(block) = self.celestia.light_block(h).await else {
                continue;
            };
            let posted = block.signed_header.header.time.unix_timestamp() as u64;
            let newest = EdenChain::signed_headers(&data)
                .into_iter()
                .filter(|hd| usable.contains(&hd.height) && hd.time_ns / 1_000_000_000 <= posted)
                .max_by_key(|hd| hd.height);
            if let Some(header) = newest {
                return Ok(Some((h, data, header)));
            }
        }
        Ok(None)
    }

    /// Every block that changed Eden's state in `(from, to]`, by bisecting on state roots.
    /// Only those need re-executing; a skipped one shows up in the enclave as a root mismatch.
    async fn changed_blocks(&self, from: u64, to: u64) -> Result<Vec<u64>> {
        let mut found = Vec::new();
        let mut stack = vec![(
            from,
            self.rpc.state_root(from).await?,
            to,
            self.rpc.state_root(to).await?,
        )];
        while let Some((lo, lo_root, hi, hi_root)) = stack.pop() {
            if lo_root == hi_root {
                continue;
            }
            if hi == lo + 1 {
                found.push(hi);
                continue;
            }
            let mid = lo + (hi - lo) / 2;
            let mid_root = self.rpc.state_root(mid).await?;
            stack.push((lo, lo_root, mid, mid_root));
            stack.push((mid, mid_root, hi, hi_root));
        }
        found.sort_unstable();
        Ok(found)
    }

    /// One block and its execution witness, shaped as the enclave's `BlockExec`.
    async fn block_for_execution(&self, number: u64) -> Result<Value> {
        use alloy_rlp::Encodable;
        let block = self
            .rpc
            .call("eth_getBlockByNumber", json!([hex_number(number), true]))
            .await?;
        let header: alloy_consensus::Header = serde_json::from_value(block.clone())
            .with_context(|| format!("block {number} header"))?;
        let mut rlp = Vec::new();
        header.encode(&mut rlp);
        let mut transactions = Vec::new();
        for tx in block["transactions"].as_array().context("transactions")? {
            transactions.push(
                self.rpc
                    .call("eth_getRawTransactionByHash", json!([tx["hash"]]))
                    .await?,
            );
        }
        let witness = self
            .rpc
            .call("debug_executionWitness", json!([hex_number(number)]))
            .await
            .with_context(|| format!("execution witness for block {number}"))?;
        Ok(json!({
            "header": format!("0x{}", hex::encode(rlp)),
            "transactions": transactions,
            "state": witness["state"],
            "codes": witness["codes"],
            "ancestors": witness["headers"],
        }))
    }
}

#[async_trait]
impl Indexer for Eden {
    async fn gather(&self, trusted: &IsmState) -> Result<Step> {
        let target = self
            .da
            .head()
            .await
            .context("celestia DA node head")?
            .saturating_sub(self.lag);
        let (store_height, trusted_block) = self.store_at(trusted.lc_store_commit, target).await?;
        if target <= store_height {
            return Ok(Step::idle(trusted.height));
        }

        if let Err(e) = self.capture_tree(trusted.height).await {
            warn!(error = %e, "could not capture an eden tree proof this tick");
        }
        let snapshot = self.cached_tree(trusted.height).with_context(|| {
            format!(
                "no captured tree proof at the trusted height {}; the route needs re-bootstrapping",
                trusted.height
            )
        })?;
        let usable: Vec<u64> = self
            .cached_heights()
            .into_iter()
            .filter(|h| *h > trusted.height)
            .collect();
        if usable.is_empty() {
            return Ok(Step::idle(trusted.height));
        }
        let (celestia_height, data, header) = self.find_post(target, &usable).await?.with_context(|| {
            format!(
                "no celestia block in the last {MAX_DA_WALK} carries an eden header at a height we hold a proof for \
                 ({}..{}); eden may have stopped posting to celestia",
                usable[0],
                usable[usable.len() - 1]
            )
        })?;

        let tree = self
            .cached_tree(header.height)
            .context("the chosen height lost its captured proof")?;
        let leaves = origin::leaves(
            &EDEN,
            &snapshot,
            trusted.state_root,
            &tree,
            B256::from(header.state_root),
        )?;
        if leaves.is_empty() {
            return Ok(Step::idle(header.height));
        }

        let changed = self
            .changed_blocks(trusted.height, header.height)
            .await
            .context("finding eden's state changes")?;
        info!(
            blocks = changed.len(),
            from = trusted.height,
            to = header.height,
            celestia = celestia_height,
            "re-executing eden"
        );
        let mut chain = Vec::with_capacity(changed.len());
        for number in changed {
            chain.push(self.block_for_execution(number).await?);
        }
        // The store the ISM moves to sits at this Celestia height; remember it ahead of the one
        // just used, so the next tick finds the store in one request either way.
        self.cache
            .write("da-heights", &format!("{celestia_height}\n{store_height}"));

        Ok(Step {
            head: header.height,
            leaves,
            chain: EDEN.name,
            input: json!({
                "celestia": {
                    "store": { "trusted": trusted_block },
                    "updates": [self.celestia.light_block(celestia_height).await?],
                },
                "proof": {
                    "dah": self.da.dah(celestia_height).await?,
                    "data": data,
                    "chain": chain,
                    "target_height": header.height,
                },
            }),
            tree,
            tree_snapshot: snapshot,
            tree_address: tee_node::evm::padded(self.hook),
        })
    }

    async fn index(&self, from: u64, to: u64) -> Result<Vec<Message>> {
        self.rpc
            .dispatched(self.mailbox, self.hook, from + 1, to)
            .await
    }

    /// Capture a tree proof now, then wait for the Celestia block that carries Eden's header at
    /// that height: the ISM has to start where a snapshot proof exists.
    async fn bootstrap(&self, identity: [u8; 32], height: Option<u64>) -> Result<IsmState> {
        let anchor = self
            .capture_tree(0)
            .await
            .context("capturing a tree proof at eden's latest block")?;
        info!(
            eden = anchor,
            "captured the anchor tree proof; waiting for DA"
        );
        for _ in 0..BOOTSTRAP_DA_TRIES {
            let head = match height {
                Some(h) => h,
                None => self.da.head().await?.saturating_sub(self.lag),
            };
            if let Some((celestia_height, _, header)) = self.find_post(head, &[anchor]).await? {
                let store = CelestiaStore {
                    trusted: self.celestia.light_block(celestia_height).await?,
                };
                self.cache.write("da-heights", &celestia_height.to_string());
                return Ok(IsmState {
                    state_root: header.state_root,
                    origin_domain: EDEN.domain,
                    height: header.height,
                    timestamp: header.time_ns / 1_000_000_000,
                    lc_store_commit: store.commitment(),
                    identity_digest: identity,
                });
            }
            if height.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        }
        anyhow::bail!("DA did not publish eden height {anchor} in time")
    }
}

/// A celestia-node DA endpoint: rows and blobs, which the consensus RPC cannot serve.
/// Untrusted: the enclave checks the rows against the `data_hash` its light client verified.
struct Da {
    url: String,
    http: reqwest::Client,
}

impl Da {
    fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("http client"),
        }
    }

    async fn call<T: serde::de::DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let response = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{method} to the DA node"))?;
        let status = response.status();
        let parsed: Value = response
            .json()
            .await
            .with_context(|| format!("{method}: DA node returned {status}"))?;
        if let Some(err) = parsed.get("error") {
            anyhow::bail!("{method}: {err}");
        }
        serde_json::from_value(parsed["result"].clone())
            .with_context(|| format!("{method}: unexpected shape"))
    }

    async fn head(&self) -> Result<u64> {
        let head: Value = self.call("header.LocalHead", json!([])).await?;
        Ok(head["header"]["height"]
            .as_str()
            .context("height")?
            .parse()?)
    }

    async fn dah(&self, height: u64) -> Result<DataAvailabilityHeader> {
        let header: Value = self.call("header.GetByHeight", json!([height])).await?;
        Ok(serde_json::from_value(header["dah"].clone())?)
    }

    async fn namespace_data(&self, height: u64) -> Result<NamespaceData> {
        debug!(height, "reading eden's namespace");
        self.call(
            "share.GetNamespaceData",
            json!([height, EdenChain::namespace()]),
        )
        .await
    }
}
