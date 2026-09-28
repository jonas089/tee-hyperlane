//! Celestia as an origin: finding the light blocks and hook proofs the enclave's
//! `celestia::Celestia` verifies. Eden rides on Celestia, so its indexer lives here too.

pub mod eden;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tee_node::celestia::{CelestiaStore, CELESTIA};
use tee_node::state::IsmState;
use tendermint::block::Height;
use tendermint_light_client_verifier::types::LightBlock;
use tendermint_rpc::{Client, HttpClient, Order, Paging};
use tracing::debug;

use crate::origin::{self, Indexer, Message, Step};

/// `tx_search` is asked over at most this many heights at a time.
const SEARCH_WINDOW: u64 = 50_000;
const INSERT_EVENT: &str = "hyperlane.core.post_dispatch.v1.EventInsertedIntoTree";
const DISPATCH_EVENT: &str = "hyperlane.core.v1.EventDispatch";
/// Public Celestia RPCs drop requests under load; a few retries turn that into latency.
const RPC_ATTEMPTS: u32 = 5;

/// `[chains.<name>]` for a Celestia chain. As Eden's parent only `rpc` and `domain` are read.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub domain: u32,
    pub rpc: String,
    /// For reads at the ISM's trusted height, which a pruning node no longer serves.
    pub archive_rpc: Option<String>,
    pub mailbox: Option<String>,
    pub merkle_tree_hook: Option<String>,
    /// How far behind head to attest. The app hash for H lives in H+1, so one is the floor;
    /// more is margin against a node reporting a head it cannot yet serve proofs for.
    #[serde(default = "default_lag")]
    pub lag: u64,
    /// As a destination: who signs, and where their key lives.
    #[serde(default = "default_chain_id")]
    pub chain_id: String,
    #[serde(default = "default_key")]
    pub key: String,
    pub home: Option<String>,
}

pub(crate) fn default_lag() -> u64 {
    2
}
fn default_chain_id() -> String {
    "teeism-local".into()
}
fn default_key() -> String {
    "relayer".into()
}

pub struct Celestia {
    rpc: Rpc,
    history: Rpc,
    hook: [u8; 32],
    lag: u64,
}

impl Celestia {
    pub fn new(config: Config) -> Result<Self> {
        let hook = config
            .merkle_tree_hook
            .context("a Celestia origin needs `merkle_tree_hook`")?;
        Ok(Self {
            history: Rpc::new(config.archive_rpc.as_deref().unwrap_or(&config.rpc))?,
            rpc: Rpc::new(&config.rpc)?,
            hook: hex::decode(hook.trim_start_matches("0x"))?
                .try_into()
                .map_err(|_| anyhow::anyhow!("merkle_tree_hook must be 32 bytes"))?,
            lag: config.lag,
        })
    }
}

#[async_trait]
impl Indexer for Celestia {
    async fn gather(&self, trusted: &IsmState) -> Result<Step> {
        let head = self.rpc.latest_height().await?.saturating_sub(self.lag);
        if head <= trusted.height + 1 {
            return Ok(Step::idle(head));
        }
        // The header that proves state at `head` is the one at `head + 1`.
        let next = self.rpc.light_block(head + 1).await?;
        let trusted_block = self.history.light_block(trusted.height + 1).await?;
        let tree = self.rpc.hook_proof(self.hook, head).await?;
        let snapshot = self
            .history
            .hook_proof(self.hook, trusted.height)
            .await
            .context("reading the tree at the trusted height; set `archive_rpc` if pruned")?;
        let head_root = CelestiaStore {
            trusted: next.clone(),
        }
        .root()?
        .state_root;
        Ok(Step {
            head,
            leaves: origin::leaves(&CELESTIA, &snapshot, trusted.state_root, &tree, head_root)?,
            chain: CELESTIA.name,
            input: json!({ "store": { "trusted": trusted_block }, "updates": [next] }),
            tree,
            tree_snapshot: snapshot,
            tree_address: self.hook,
        })
    }

    async fn index(&self, from: u64, to: u64) -> Result<Vec<Message>> {
        self.history.dispatched(from + 1, to, self.hook).await
    }

    async fn bootstrap(&self, identity: [u8; 32], height: Option<u64>) -> Result<IsmState> {
        let anchor = match height {
            Some(h) => h,
            None => self
                .rpc
                .latest_height()
                .await?
                .saturating_sub(self.lag)
                .max(2),
        };
        let store = CelestiaStore {
            trusted: self.rpc.light_block(anchor).await?,
        };
        let root = store.root()?;
        Ok(IsmState {
            state_root: root.state_root.0,
            origin_domain: CELESTIA.domain,
            height: root.height,
            timestamp: root.timestamp,
            lc_store_commit: store.commitment(),
            identity_digest: identity,
        })
    }
}

/// A Celestia consensus node's RPC. Untrusted: light blocks are re-verified, proofs re-checked.
pub struct Rpc {
    client: HttpClient,
}

impl Rpc {
    pub fn new(url: &str) -> Result<Self> {
        Ok(Self {
            client: HttpClient::new(url).context("celestia rpc")?,
        })
    }

    async fn retrying<T, F, Fut>(what: &str, call: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, tendermint_rpc::Error>>,
    {
        for attempt in 1.. {
            match call().await {
                Ok(value) => return Ok(value),
                Err(e) if attempt < RPC_ATTEMPTS => {
                    debug!(call = what, attempt, error = %e, "retrying celestia rpc");
                    tokio::time::sleep(std::time::Duration::from_millis(250 * u64::from(attempt)))
                        .await;
                }
                Err(e) => return Err(e).with_context(|| what.to_string()),
            }
        }
        unreachable!()
    }

    pub async fn latest_height(&self) -> Result<u64> {
        Ok(Self::retrying("status", || self.client.status())
            .await?
            .sync_info
            .latest_block_height
            .value())
    }

    /// The header at `height`, its commit, and the validator sets at `height` and `height + 1`.
    pub async fn light_block(&self, height: u64) -> Result<LightBlock> {
        let (h, next) = (Height::try_from(height)?, Height::try_from(height + 1)?);
        let commit = Self::retrying("commit", || self.client.commit(h)).await?;
        let validators =
            Self::retrying("validators", || self.client.validators(h, Paging::All)).await?;
        let next_validators =
            Self::retrying("validators", || self.client.validators(next, Paging::All)).await?;
        let peer = Self::retrying("status", || self.client.status())
            .await?
            .node_info
            .id;
        Ok(LightBlock::new(
            commit.signed_header,
            tendermint::validator::Set::new(validators.validators, None),
            tendermint::validator::Set::new(next_validators.validators, None),
            peer,
        ))
    }

    /// The hook's stored bytes at `height` with the store proof, shaped as the enclave's
    /// `celestia::TreeProof`.
    pub async fn hook_proof(&self, hook: [u8; 32], height: u64) -> Result<Value> {
        // Prefix 2,4 (post-dispatch submodule 2, collection 4), then the hook's internal id.
        let key = [[2u8, 4].as_slice(), &hook[24..]].concat();
        let at = Height::try_from(height)?;
        let r = Self::retrying("abci_query for the merkle tree hook", || {
            self.client.abci_query(
                Some("/store/hyperlane/key".into()),
                key.clone(),
                Some(at),
                true,
            )
        })
        .await?;
        anyhow::ensure!(
            r.height.value() == height,
            "node answered at height {} rather than {height}",
            r.height
        );
        let steps: Vec<Value> = r
            .proof
            .context("node returned no proof; it may not have `prove` enabled")?
            .ops
            .into_iter()
            .map(|op| json!({ "proof_type": op.field_type, "key": op.key, "data": op.data }))
            .collect();
        Ok(json!({ "hook_id": hook, "hook_bytes": r.value, "steps": steps }))
    }

    /// Every message inserted into `hook`'s tree over `from..=to`, in tree order, from the
    /// dispatch and insert events of the transactions that did it.
    pub async fn dispatched(&self, from: u64, to: u64, hook: [u8; 32]) -> Result<Vec<Message>> {
        let mut found = Vec::new();
        let mut start = from;
        while start <= to {
            let end = (start + SEARCH_WINDOW - 1).min(to);
            let query: tendermint_rpc::query::Query = format!(
                "tx.height >= {start} AND tx.height <= {end} AND {INSERT_EVENT}.index EXISTS"
            )
            .parse()?;
            for page in 1.. {
                let results = Self::retrying("tx_search", || {
                    self.client
                        .tx_search(query.clone(), false, page, 100, Order::Ascending)
                })
                .await?;
                for tx in &results.txs {
                    found.extend(inserts(&tx.tx_result.events, hook)?);
                }
                if results.txs.len() < 100 {
                    break;
                }
            }
            start = end + 1;
        }
        found.sort_by_key(|(index, _)| *index);
        Ok(found.into_iter().map(|(_, m)| m).collect())
    }
}

/// The leaves one transaction inserted into `hook`'s tree, with their indexes. Inserts into
/// any other hook on the chain are skipped: counting them was a live failure, when a second
/// deployment's hook shared the chain.
fn inserts(events: &[tendermint::abci::Event], hook: [u8; 32]) -> Result<Vec<(u32, Message)>> {
    let attribute = |ev: &tendermint::abci::Event, key: &str| {
        ev.attributes.iter().find_map(|a| {
            (a.key_str().ok()? == key)
                .then(|| a.value_str().ok().map(|v| v.trim_matches('"').to_string()))?
        })
    };
    let decode = |s: String| hex::decode(s.trim_start_matches("0x"));
    let mut bodies = Vec::new();
    let mut out = Vec::new();
    for ev in events {
        if ev.kind == DISPATCH_EVENT {
            if let Some(m) = attribute(ev, "message") {
                bodies.push(decode(m)?);
            }
        } else if ev.kind == INSERT_EVENT {
            if attribute(ev, "merkle_tree_hook_id")
                .map(decode)
                .transpose()?
                .is_some_and(|id| id != hook)
            {
                continue;
            }
            let index: u32 = attribute(ev, "index")
                .context("insert event without index")?
                .parse()?;
            let id: [u8; 32] =
                decode(attribute(ev, "message_id").context("insert event without id")?)?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("message id is not 32 bytes"))?;
            let bytes = bodies
                .iter()
                .find(|b| alloy_primitives::keccak256(b).0 == id)
                .cloned()
                .unwrap_or_default();
            out.push((index, Message { id, bytes }));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::inserts;
    use tendermint::abci::{Event, EventAttribute};

    const OURS: [u8; 32] = [0x11; 32];
    const THEIRS: [u8; 32] = [0x22; 32];

    fn insert(hook: [u8; 32], index: u32) -> Event {
        let attr = |k: &str, v: String| EventAttribute::from((k, v, true));
        Event {
            kind: super::INSERT_EVENT.to_string(),
            attributes: vec![
                attr("index", index.to_string()),
                attr(
                    "merkle_tree_hook_id",
                    format!("\"0x{}\"", hex::encode(hook)),
                ),
                attr(
                    "message_id",
                    format!("\"0x{}\"", hex::encode([index as u8; 32])),
                ),
            ],
        }
    }

    /// Only leaves in our own tree count, even when a block carries inserts into another.
    #[test]
    fn only_inserts_into_our_hook_count() {
        let events = vec![insert(THEIRS, 1), insert(OURS, 2), insert(THEIRS, 3)];
        let found = inserts(&events, OURS).unwrap();
        assert_eq!(found.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![2]);
    }
}
