//! The relayer loop: one per route.
//!
//! Interval-driven and sequential. There is no event subscription, no reorg handling and no
//! speculative work, because the ISM state on the destination chain is the only progress
//! marker - restarting is the same as continuing.
//!
//! Each tick does the whole pipeline for one route, or nothing:
//!
//!   attest   ask the enclave to verify the origin and sign the result
//!   prove    two Groth16 proofs of that one attestation, on local CPU
//!   submit   advance the ISM, authorise the batch, deliver the messages
//!
//! Proving is minutes of CPU, so all routes share one permit. Two routes competing for cores
//! would only make both slower.
//!
//! ## Why a message cannot be skipped
//!
//! The enclave replays the origin's merkle tree from the ISM's trusted height using the ids
//! in the batch, and checks the result against the tree it just proved at the new height. A
//! batch that omits, reorders or invents a leaf produces a different root, and the
//! attestation fails. So within one attestation, completeness is cryptographic rather than a
//! property of this loop.
//!
//! That leaves exactly one way to lose a message: abandon a batch *after* `updateState` has
//! advanced the trusted height past it. The next attestation would start from a snapshot
//! that already contains those leaves, so they would never appear in any batch again. This
//! loop therefore finishes an in-flight batch before it starts a new one, and the submit
//! scripts skip whatever already happened on chain, so resuming is always safe.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::chains::{read_ism_state, Destination};
use crate::commands;
use crate::config::{ChainConfig, RouteConfig};

/// Proving is CPU-bound and local by policy, so routes take turns.
pub fn cpu_prover_permit() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(1))
}

/// Where proved batches are kept, so a crash after proving does not discard the work - and
/// so the attestation API has something to serve.
pub struct ProofStore {
    root: PathBuf,
}

impl ProofStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path_for(&self, route: &str, height: u64) -> PathBuf {
        self.root.join(route).join(format!("{height}.json"))
    }

    pub fn prepare(&self, route: &str) -> Result<()> {
        std::fs::create_dir_all(self.root.join(route).join("staging"))?;
        Ok(())
    }

    /// Scratch space for a batch in flight. Kept out of the finished directory so the
    /// attestation API never serves a half-written record.
    pub fn staging(&self, route: &str, name: &str) -> PathBuf {
        self.root.join(route).join("staging").join(name)
    }
}

/// Drive one route forever.
pub async fn run_route(
    route: RouteConfig,
    store: Arc<ProofStore>,
    cpu: Arc<Semaphore>,
    tick: Duration,
) {
    store.prepare(&route.name).ok();
    let mut failures: u32 = 0;

    loop {
        match advance(&route, &store, &cpu).await {
            Ok(Some(height)) => {
                failures = 0;
                info!(route = %route.name, height, "batch delivered");
            }
            Ok(None) => failures = 0,
            // Most failures are transient: the head has not moved, an RPC is down, finality
            // has not caught up. A few are not, and a batch that can never succeed would
            // otherwise retry every tick forever. Backing off keeps the log readable and the
            // gas estimation calls rare, while still reporting every attempt.
            Err(e) => {
                failures = failures.saturating_add(1);
                warn!(
                    route = %route.name,
                    error = %e,
                    consecutive_failures = failures,
                    retry_in_secs = backoff(tick, failures).as_secs(),
                    "tick failed"
                );
            }
        }
        tokio::time::sleep(backoff(tick, failures)).await;
    }
}

/// Wait before the next attempt: the normal tick while healthy, doubling per consecutive
/// failure up to a ceiling. The ceiling matters more than the curve - a route that is broken
/// should still notice within the hour when whatever broke it is fixed.
fn backoff(tick: Duration, failures: u32) -> Duration {
    if failures == 0 {
        return tick;
    }
    let doublings = failures.min(MAX_BACKOFF_DOUBLINGS);
    // The ceiling is never below the normal cadence: backing off must not make a route retry
    // sooner than it would have while healthy.
    let ceiling = MAX_BACKOFF.max(tick);
    tick.saturating_mul(1 << doublings).min(ceiling)
}

const MAX_BACKOFF_DOUBLINGS: u32 = 8;
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// One pass. Returns the height delivered, or `None` when there was nothing to do.
///
/// The stages are the same functions the `attest-*`, `prove` and submit subcommands call,
/// so a route that misbehaves here can be stepped through by hand with identical results.
async fn advance(
    route: &RouteConfig,
    store: &ProofStore,
    cpu: &Arc<Semaphore>,
) -> Result<Option<u64>> {
    // A batch left over from a crash is finished first. Attesting past it would strand its
    // messages permanently - see the module comment.
    if let Some(height) = finish_staged_batch(route, store)? {
        return Ok(Some(height));
    }

    let trusted = read_ism_state(&route.destination, &route.ism_id)?;

    let attestation = store.staging(&route.name, "attestation.json");
    let attested = match &route.origin {
        ChainConfig::Celestia { rpc, archive_rpc, merkle_tree_hook_id, .. } => {
            commands::attest_celestia(
                rpc,
                archive_rpc.as_deref(),
                &route.tee_node_url,
                &trusted,
                merkle_tree_hook_id,
                DEFAULT_LAG,
                Some(path_string(&attestation)),
            )
            .await
        }
        ChainConfig::Ethereum {
            execution_rpc,
            archive_rpc,
            beacon_rpc,
            mailbox,
            merkle_tree_hook,
            merkle_tree_base_slot,
            ..
        } => {
            let origin_field = |name: &str| format!("an Ethereum origin needs `{name}` in its route config");
            let beacon_rpc = beacon_rpc.as_deref().with_context(|| origin_field("beacon_rpc"))?;
            let merkle_tree_hook = merkle_tree_hook
                .as_deref()
                .with_context(|| origin_field("merkle_tree_hook"))?;
            let merkle_tree_base_slot = merkle_tree_base_slot
                .with_context(|| origin_field("merkle_tree_base_slot"))?;
            commands::attest_ethereum(
                beacon_rpc,
                execution_rpc,
                archive_rpc.as_deref(),
                &route.tee_node_url,
                route.checkpoint.as_deref(),
                &trusted,
                merkle_tree_hook,
                mailbox,
                merkle_tree_base_slot,
                Some(path_string(&attestation)),
            )
            .await
        }
        // An L2 origin is Ethereum's flow plus the storage proof that says which L2 block
        // Ethereum has confirmed.
        ChainConfig::EthereumL2 {
            l2_rpc,
            l1,
            rollup,
            l1_anchor_contract,
            mailbox,
            merkle_tree_hook,
            merkle_tree_base_slot,
            ..
        } => {
            let ChainConfig::Ethereum { execution_rpc, beacon_rpc, archive_rpc, .. } = &**l1
            else {
                anyhow::bail!("an L2 origin's `l1` must be an Ethereum chain")
            };
            let beacon_rpc = beacon_rpc
                .as_deref()
                .context("an L2 origin's `l1` needs `beacon_rpc`")?;
            // The rollup's storage is proven at the finalized L1 block, which is already
            // outside a public node's window.
            let l1_execution = archive_rpc.as_deref().unwrap_or(execution_rpc);
            commands::attest_l2(
                commands::L2Kind::parse(rollup)?,
                beacon_rpc,
                l1_execution,
                l2_rpc,
                &route.tee_node_url,
                &trusted,
                l1_anchor_contract,
                merkle_tree_hook,
                mailbox,
                *merkle_tree_base_slot,
                Some(path_string(&attestation)),
            )
            .await
        }
    };

    // "Nothing dispatched" is the common case and not worth a warning every tick.
    if let Err(error) = attested {
        let quiet = error.to_string().contains("nothing to attest")
            || error.to_string().contains("has not advanced")
            || error.to_string().contains("has not passed");
        return if quiet { Ok(None) } else { Err(error) };
    }

    // Proving is minutes of CPU, so routes take turns rather than competing for cores.
    let _permit = cpu.acquire().await?;
    let proved = store.staging(&route.name, "proved.json");
    commands::prove(
        &path_string(&attestation),
        &elf_dir(),
        &path_string(&proved),
    )
    .await?;

    let height = submit_and_file(route, store, &proved)?;
    Ok(Some(height))
}

/// Submit a proved batch, move it into the finished directory where the API serves it, and
/// clear the staging area.
///
/// Clearing matters for more than tidiness: a leftover `attestation.json` is what the status
/// endpoint reads to say a route is proving, so one left behind reports a finished batch as
/// still in flight.
fn submit_and_file(route: &RouteConfig, store: &ProofStore, proved: &Path) -> Result<u64> {
    Destination::new(route.destination.clone(), route.ism_id.clone()).submit(proved)?;

    let height = std::fs::read(proved)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .and_then(|record| record["height"].as_u64())
        .unwrap_or_default();

    std::fs::rename(proved, store.path_for(&route.name, height))?;
    let _ = std::fs::remove_file(store.staging(&route.name, "attestation.json"));
    Ok(height)
}

/// Resubmit a batch that was proved but never finished. Returns its height if there was one.
fn finish_staged_batch(route: &RouteConfig, store: &ProofStore) -> Result<Option<u64>> {
    let proved = store.staging(&route.name, "proved.json");
    if !proved.exists() {
        return Ok(None);
    }
    warn!(route = %route.name, "resuming a batch left unfinished by an earlier run");
    submit_and_file(route, store, &proved).map(Some)
}

/// How far behind a Celestia head to attest. The app hash for height H lives in H+1.
const DEFAULT_LAG: u64 = 8;

fn elf_dir() -> String {
    std::env::var("TEE_HYPERLANE_ELF_DIR")
        .unwrap_or_else(|_| "../tee-circuit/elf".to_string())
}

fn path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_stops() {
        let tick = Duration::from_secs(120);
        assert_eq!(backoff(tick, 0), tick, "a healthy route keeps its normal cadence");
        assert_eq!(backoff(tick, 1), Duration::from_secs(240));
        assert_eq!(backoff(tick, 3), Duration::from_secs(960));

        // Capped, so a route broken overnight still retries within the hour once it is fixed.
        assert_eq!(backoff(tick, 20), MAX_BACKOFF);
    }

    #[test]
    fn a_long_tick_is_never_shortened_by_backing_off() {
        let tick = Duration::from_secs(45 * 60);
        assert_eq!(backoff(tick, 5), tick.max(MAX_BACKOFF));
    }
}
