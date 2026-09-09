//! Ethereum as an origin: anchor an ISM to a checkpoint, then walk it forward.
//!
//! The light-client store is never stored. It is rebuilt each tick from the ISM's own
//! trusted state, which is what lets a route resume after any outage without memory.

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::ethereum::SECONDS_PER_SLOT;

use super::evm_tree_input;

/// How far back to look for the checkpoint an L2-origin ISM's store was built from. Eight
/// epochs is about 51 minutes, far longer than a tick.
const SLOTS_PER_EPOCH: u64 = 32;
const MAX_CHECKPOINT_SEARCH_EPOCHS: u64 = 8;

/// Rebuild the exact light-client store an ISM committed to.
///
/// The commitment covers the finalized header, which moves every epoch, so the checkpoint an
/// ISM was bootstrapped from only reconstructs its genesis store. The checkpoint has to be
/// recovered instead - and how depends on what the ISM's timestamp means.
///
/// For an Ethereum-origin ISM it is exact: post-merge, an execution payload's timestamp is
/// `genesis_time + slot * 12`, so the trusted timestamp names the slot and the slot names the
/// block root. For an L2-origin ISM the timestamp is the *L2's*, which says nothing about L1,
/// so the finalized checkpoints are walked backwards until one reproduces the commitment.
/// Either way the route needs no memory of its own, which is what makes it resumable.
pub(super) async fn rebuild_ethereum_store(
    beacon: &crate::ethereum::EthereumReader,
    config: &crate::ethereum::ChainConfig,
    trusted: &tee_attestation::IsmState,
    explicit: Option<&str>,
) -> Result<(tee_node::origins::ethereum::EthereumStore, String)> {
    use tee_node::origins::ethereum::commit_ethereum_store;

    // A configured checkpoint is a hint, not an answer, and it stops being the right one the
    // moment the route succeeds: the ISM's commitment is to whatever store the last update
    // left behind, while this names the store the ISM was created with. It is still worth
    // trying first, because that genesis store is the one case the search below cannot reach
    // - the search only walks back eight finalized epochs, and a genesis anchor is usually
    // older than that. So try it, and fall through rather than failing when it no longer
    // matches, which is exactly what an advanced ISM looks like.
    if let Some(checkpoint) = explicit {
        if let Ok(store) = bootstrap_store(beacon, config, checkpoint).await {
            if commit_ethereum_store(&store) == trusted.lc_store_commit {
                return Ok((store, checkpoint.to_string()));
            }
            debug!(
                checkpoint,
                "the configured checkpoint is not this ISM's store; searching from the head"
            );
        }
    }

    if trusted.origin_domain == tee_node::origins::Origin::Ethereum.domain() {
        let slot = trusted.timestamp.saturating_sub(config.genesis_time) / SECONDS_PER_SLOT;
        let checkpoint = beacon
            .block_root_at_slot(slot)
            .await
            .with_context(|| format!("no beacon block at slot {slot}"))?;
        let store = bootstrap_store(beacon, config, &checkpoint).await?;
        anyhow::ensure!(
            commit_ethereum_store(&store) == trusted.lc_store_commit,
            "store rebuilt from {checkpoint} does not match the ISM's commitment"
        );
        return Ok((store, checkpoint));
    }

    let head = beacon.finalized_slot().await?;
    for epoch in 0..MAX_CHECKPOINT_SEARCH_EPOCHS {
        let slot = head.saturating_sub(epoch * SLOTS_PER_EPOCH);
        let Ok(checkpoint) = beacon.block_root_at_slot(slot).await else {
            continue;
        };
        let Ok(store) = bootstrap_store(beacon, config, &checkpoint).await else {
            continue;
        };
        if commit_ethereum_store(&store) == trusted.lc_store_commit {
            return Ok((store, checkpoint));
        }
    }
    anyhow::bail!(
        "no finalized checkpoint in the last {MAX_CHECKPOINT_SEARCH_EPOCHS} epochs rebuilds \
         this ISM's light-client store; pass `checkpoint` in the route config"
    )
}

pub(super) async fn bootstrap_store(
    beacon: &crate::ethereum::EthereumReader,
    config: &crate::ethereum::ChainConfig,
    checkpoint: &str,
) -> Result<tee_node::origins::ethereum::EthereumStore> {
    use helios_consensus_core::{apply_bootstrap, verify_bootstrap};
    use tee_node::origins::ethereum::EthereumStore;

    let bootstrap = beacon.bootstrap(checkpoint).await?;
    let root: alloy_primitives::B256 = checkpoint.parse()?;
    verify_bootstrap::<crate::ethereum::Spec>(&bootstrap, root, &config.forks)
        .map_err(|e| anyhow::anyhow!("bootstrap does not match checkpoint {checkpoint}: {e}"))?;

    let mut inner = helios_consensus_core::types::LightClientStore::default();
    apply_bootstrap::<crate::ethereum::Spec>(&mut inner, &bootstrap);
    Ok(EthereumStore {
        store: inner,
        genesis_root: config.genesis_root,
        genesis_time: config.genesis_time,
        forks: config.forks.clone(),
    })
}

/// Gather one Ethereum step and hand it to the enclave.
#[allow(clippy::too_many_arguments)]
pub async fn attest_ethereum(
    beacon: &str,
    execution: &str,
    archive: Option<&str>,
    enclave_url: &str,
    checkpoint: Option<&str>,
    trusted_state_hex: &str,
    merkle_tree_hook: &str,
    mailbox: &str,
    base_slot: u64,
    out: Option<String>,
) -> Result<()> {
    use crate::enclave::EnclaveClient;
    use crate::ethereum::{EthereumReader, ExecutionReader};

    let trusted_raw = hex::decode(trusted_state_hex.trim_start_matches("0x"))?;
    let trusted = tee_attestation::decode_ism_state(&trusted_raw)?;

    let beacon_reader = EthereumReader::new(beacon);
    let config = beacon_reader.chain_config().await?;
    let (store, _checkpoint) =
        rebuild_ethereum_store(&beacon_reader, &config, &trusted, checkpoint).await?;

    let finality = beacon_reader.finality_update().await?;

    let hook: alloy_primitives::Address = merkle_tree_hook.parse()?;
    let mailbox_address: alloy_primitives::Address = mailbox.parse()?;
    let exec = ExecutionReader::new(execution);
    // Reads at the trusted height are historical. A public node keeps state proofs for about
    // 128 blocks, so resuming after a longer outage needs an archive node - without one the
    // route is stuck rather than merely behind.
    let history = ExecutionReader::new(archive.unwrap_or(execution));

    // The finalized execution block the enclave will attest.
    let target = finality
        .finalized_header()
        .execution()
        .map_err(|_| anyhow::anyhow!("finalized header has no execution payload"))?;
    let target_block = *target.block_number();
    super::record_attestable_head(out.as_deref(), target_block);
    anyhow::ensure!(
        target_block > trusted.height,
        "finalized head {target_block} has not passed the trusted height {}",
        trusted.height
    );

    let tree_proof = exec
        .merkle_tree_proof(hook, base_slot, target_block)
        .await?;
    // Sent as a proof, not as a decoded tree: the enclave re-reads it under the ISM's own
    // `state_root`, so it is the ISM that decides where the replay starts.
    let snapshot_proof = history
        .merkle_tree_proof(hook, base_slot, trusted.height)
        .await
        .context("reading the merkle tree at the trusted height; set `archive_rpc` if pruned")?;

    let dispatched = history
        .dispatched_messages(mailbox_address, hook, trusted.height + 1, target_block)
        .await?;
    info!(
        height = target_block,
        trusted = trusted.height,
        leaves = dispatched.len(),
        "attesting ethereum"
    );
    anyhow::ensure!(!dispatched.is_empty(), "nothing to attest");

    let mut tree_address = [0u8; 32];
    tree_address[12..].copy_from_slice(hook.as_slice());

    // TreeInput is an internally-tagged enum, so the variant's fields sit alongside `kind`.
    let tree_input = evm_tree_input(&tree_proof)?;
    let snapshot_input = evm_tree_input(&snapshot_proof)?;

    let request = serde_json::json!({
        "protocol": tee_node::attest::PROTOCOL_VERSION,
        "trusted_state": hex::encode(&trusted_raw),
        "origin": {
            "chain": "ethereum",
            "store": store,
            "updates": { "committee_updates": [], "finality_update": finality },
        },
        "tree": tree_input,
        "tree_snapshot": snapshot_input,
        "message_ids": dispatched.iter().map(|d| d.message_id).collect::<Vec<_>>(),
        "merkle_tree_address": tree_address,
    });

    let attestation = EnclaveClient::new(enclave_url).attest(&request).await?;
    info!(messages = attestation.message_ids.len(), "enclave attested");

    if let Some(path) = out {
        let record = serde_json::json!({
            "attestation": {
                "quote": attestation.quote,
                "event_log": attestation.event_log,
                "payload": attestation.payload,
                "new_state": attestation.new_state,
            },
            "messages": dispatched.iter().map(|d| hex::encode(&d.message)).collect::<Vec<_>>(),
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&record)?)?;
        debug!(path, "wrote attestation");
    }
    Ok(())
}

/// Anchor an Ethereum-origin ISM to a sync-committee checkpoint.
pub async fn bootstrap_ethereum(
    beacon: &str,
    execution: &str,
    checkpoint: Option<String>,
    identity_digest: &str,
) -> Result<()> {
    use crate::ethereum::EthereumReader;
    use helios_consensus_core::{apply_bootstrap, verify_bootstrap};
    use tee_node::origins::ethereum::{commit_ethereum_store, ethereum_root, EthereumStore};

    let reader = EthereumReader::new(beacon);
    let config = reader.chain_config().await?;
    let checkpoint = match checkpoint {
        Some(c) => c,
        None => reader.finalized_root().await?,
    };
    let bootstrap = reader.bootstrap(&checkpoint).await?;

    // Check the bootstrap against the checkpoint before believing any of it. This is the
    // one place trust enters, and it enters by explicit choice of block root.
    let root: alloy_primitives::B256 = checkpoint.parse()?;
    verify_bootstrap::<crate::ethereum::Spec>(&bootstrap, root, &config.forks)
        .map_err(|e| anyhow::anyhow!("bootstrap does not match checkpoint {checkpoint}: {e}"))?;

    let mut inner = helios_consensus_core::types::LightClientStore::default();
    apply_bootstrap::<crate::ethereum::Spec>(&mut inner, &bootstrap);

    let store = EthereumStore {
        store: inner,
        genesis_root: config.genesis_root,
        genesis_time: config.genesis_time,
        forks: config.forks,
    };
    let head = ethereum_root(&store)?;

    let digest = hex::decode(identity_digest.trim_start_matches("0x"))?;
    let state = tee_attestation::IsmState {
        state_root: head.state_root.0,
        origin_domain: tee_node::origins::Origin::Ethereum.domain(),
        height: head.height,
        timestamp: head.timestamp,
        lc_store_commit: commit_ethereum_store(&store),
        identity_digest: digest
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("identity digest must be 32 bytes"))?,
    };

    let _ = execution;
    println!("checkpoint       {checkpoint}");
    println!("execution block  {}", state.height);
    println!("timestamp        {}", state.timestamp);
    println!("state root       0x{}", hex::encode(state.state_root));
    println!("lc store commit  0x{}", hex::encode(state.lc_store_commit));
    println!();
    println!(
        "genesis state    0x{}",
        hex::encode(tee_attestation::encode_ism_state(&state))
    );
    Ok(())
}
