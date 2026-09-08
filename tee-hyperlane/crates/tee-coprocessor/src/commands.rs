//! What each subcommand actually does.
//!
//! Split from `main.rs` so that file stays a description of the command line and nothing
//! else. Each function here is one stage of the pipeline, and each is runnable on its own -
//! which is what makes a stuck route debuggable.

use std::time::Duration;

use anyhow::{Context, Result};
use tracing::info;

use crate::config::Config;
use crate::ethereum::SECONDS_PER_SLOT;
use crate::tasks::{cpu_prover_permit, run_route, ProofStore};

/// Anchor a Celestia-origin ISM to a live header.
pub async fn bootstrap_celestia(
    rpc: &str,
    lag: u64,
    height: Option<u64>,
    identity_digest: &str,
) -> Result<()> {
    use crate::celestia::CelestiaReader;
    use tee_node::origins::celestia::{commit_celestia_store, get_celestia_root, CelestiaStore};

    let reader = CelestiaReader::new(rpc)?;
    let anchor = match height {
        Some(h) => h,
        None => reader.latest_height().await?.saturating_sub(lag).max(2),
    };
    let trusted = reader.light_block(anchor).await?;
    let store = CelestiaStore { trusted };
    let root = get_celestia_root(&store)?;

    let digest = hex::decode(identity_digest.trim_start_matches("0x"))?;
    let state = tee_attestation::IsmState {
        state_root: root.state_root.0,
        origin_domain: tee_node::origins::Origin::Celestia.domain(),
        height: root.height,
        timestamp: root.timestamp,
        lc_store_commit: commit_celestia_store(&store),
        identity_digest: digest
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("identity digest must be 32 bytes"))?,
    };

    let header = &store.trusted.signed_header.header;
    println!("chain            {}", header.chain_id);
    println!("trusted header   {}", store.trusted.height());
    println!("app hash commits to state at height {}", state.height);
    println!("timestamp        {}", state.timestamp);
    println!("state root       0x{}", hex::encode(state.state_root));
    println!("lc store commit  0x{}", hex::encode(state.lc_store_commit));
    println!();
    println!("genesis state    0x{}", hex::encode(tee_attestation::encode_ism_state(&state)));
    Ok(())
}

/// Gather one Ethereum step and hand it to the enclave.
#[allow(clippy::too_many_arguments)]
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
async fn rebuild_ethereum_store(
    beacon: &crate::ethereum::EthereumReader,
    config: &crate::ethereum::ChainConfig,
    trusted: &tee_attestation::IsmState,
    explicit: Option<&str>,
) -> Result<(tee_node::origins::ethereum::EthereumStore, String)> {
    use tee_node::origins::ethereum::commit_ethereum_store;

    if let Some(checkpoint) = explicit {
        let store = bootstrap_store(beacon, config, checkpoint).await?;
        anyhow::ensure!(
            commit_ethereum_store(&store) == trusted.lc_store_commit,
            "store rebuilt from {checkpoint} does not match the ISM's commitment"
        );
        return Ok((store, checkpoint.to_string()));
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

async fn bootstrap_store(
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

/// How far back to look for the checkpoint an L2-origin ISM's store was built from. Eight
/// epochs is about 51 minutes, far longer than a tick.
const SLOTS_PER_EPOCH: u64 = 32;
const MAX_CHECKPOINT_SEARCH_EPOCHS: u64 = 8;

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
    use crate::ethereum::{expected_current_slot, EthereumReader, ExecutionReader};

    let trusted_raw = hex::decode(trusted_state_hex.trim_start_matches("0x"))?;
    let trusted = tee_attestation::decode_ism_state(&trusted_raw)?;

    let beacon_reader = EthereumReader::new(beacon);
    let config = beacon_reader.chain_config().await?;
    let (store, _checkpoint) =
        rebuild_ethereum_store(&beacon_reader, &config, &trusted, checkpoint).await?;

    let finality = beacon_reader.finality_update().await?;
    let slot = expected_current_slot(config.genesis_time);

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
    anyhow::ensure!(
        target_block > trusted.height,
        "finalized head {target_block} has not passed the trusted height {}",
        trusted.height
    );

    let tree_proof = exec.merkle_tree_proof(hook, base_slot, target_block).await?;
    let snapshot_proof = history
        .merkle_tree_proof(hook, base_slot, trusted.height)
        .await
        .context("reading the merkle tree at the trusted height; set `archive_rpc` if pruned")?;
    let snapshot = tee_node::hyperlane_state::get_evm_merkle_tree(
        alloy_primitives::B256::from(trusted.state_root),
        &snapshot_proof,
    )?;

    let dispatched = history
        .dispatched_messages(mailbox_address, hook, trusted.height + 1, target_block)
        .await?;
    println!(
        "finalized {target_block} | trusted {} | {} new leaves",
        trusted.height,
        dispatched.len()
    );
    anyhow::ensure!(!dispatched.is_empty(), "nothing to attest");

    let mut tree_address = [0u8; 32];
    tree_address[12..].copy_from_slice(hook.as_slice());

    // TreeInput is an internally-tagged enum, so the variant's fields sit alongside `kind`.
    let mut tree_input = serde_json::to_value(&tree_proof)?;
    tree_input
        .as_object_mut()
        .context("tree proof must be an object")?
        .insert("kind".into(), serde_json::json!("evm"));

    let request = serde_json::json!({
        "trusted_state": hex::encode(&trusted_raw),
        "origin": {
            "chain": "ethereum",
            "store": store,
            "updates": { "committee_updates": [], "finality_update": finality },
            "expected_current_slot": slot,
        },
        "tree": tree_input,
        "tree_snapshot": snapshot,
        "message_ids": dispatched.iter().map(|d| d.message_id).collect::<Vec<_>>(),
        "merkle_tree_address": tree_address,
    });

    let attestation = EnclaveClient::new(enclave_url).attest(&request).await?;
    println!("attested new state 0x{}", attestation.new_state);
    println!("messages           {}", attestation.message_ids.len());

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
        println!("wrote {path}");
    }
    Ok(())
}

/// Produce the genesis ISM state for an Arbitrum-origin ISM.
///
/// The anchor is the same weak-subjectivity checkpoint an Ethereum-origin ISM uses, because
/// an L2 origin rides Ethereum's light client: the state carries *Ethereum's* store
/// commitment, and the height and timestamp of the L2 block Ethereum has confirmed.
pub async fn bootstrap_l2(
    kind: L2Kind,
    beacon: &str,
    l1_execution: &str,
    l2_archive: &str,
    anchor: &str,
    checkpoint: Option<String>,
    identity_digest: &str,
) -> Result<()> {
    use crate::ethereum::{EthereumReader, ExecutionReader};
    use tee_node::origins::ethereum::{commit_ethereum_store, get_ethereum_root};

    let beacon_reader = EthereumReader::new(beacon);
    let config = beacon_reader.chain_config().await?;
    let checkpoint = match checkpoint {
        Some(explicit) => explicit,
        None => beacon_reader.finalized_root().await?,
    };
    let store = bootstrap_store(&beacon_reader, &config, &checkpoint).await?;

    let l1_block = get_ethereum_root(&store)?.height;
    let l1 = ExecutionReader::new(l1_execution);
    let l2 = ExecutionReader::new(l2_archive);

    let l1_state_root: alloy_primitives::B256 = l1
        .call(
            "eth_getBlockByNumber",
            serde_json::json!([format!("0x{l1_block:x}"), false]),
        )
        .await?["stateRoot"]
        .as_str()
        .context("L1 block has no state root")?
        .parse()?;
    let (head, _) = get_l2_root(kind, &l1, &l2, anchor, l1_block, l1_state_root).await?;

    let digest = hex::decode(identity_digest.trim_start_matches("0x"))?;
    let state = tee_attestation::IsmState {
        state_root: head.state_root.0,
        origin_domain: kind.domain(),
        height: head.height,
        timestamp: head.timestamp,
        lc_store_commit: commit_ethereum_store(&store),
        identity_digest: digest
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("identity digest must be 32 bytes"))?,
    };

    println!("checkpoint         {checkpoint}");
    println!("L1 block           {l1_block}");
    println!("confirmed L2 block {}", state.height);
    println!("timestamp          {}", state.timestamp);
    println!("state root         0x{}", hex::encode(state.state_root));
    println!();
    println!("genesis state      0x{}", hex::encode(tee_attestation::encode_ism_state(&state)));
    Ok(())
}

/// Gather one Arbitrum step and hand it to the enclave.
///
/// An L2 origin is Ethereum's flow with two extra links: the L1 storage proof that says which
/// assertion Ethereum confirmed, and the L2 header that assertion commits to. The tree is
/// then read under the L2 state root exactly as it is under Ethereum's.
///
/// The L2 reads need an archive endpoint. The confirmed assertion is thousands of L2 blocks
/// behind head - that lag is the rollup's challenge window, not something to tune - and no
/// public node keeps state that far back.
/// Which rollup an L2 origin is, and what it needs to name its anchor contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L2Kind {
    Arbitrum,
    Base,
}

impl L2Kind {
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "arbitrum" => Ok(Self::Arbitrum),
            "base" => Ok(Self::Base),
            other => anyhow::bail!("unknown L2 `{other}`; expected arbitrum or base"),
        }
    }

    fn domain(&self) -> u32 {
        match self {
            Self::Arbitrum => tee_node::origins::Origin::Arbitrum.domain(),
            Self::Base => tee_node::origins::Origin::Base.domain(),
        }
    }

    /// The tag the enclave's `OriginInput` is deserialised by.
    fn chain_tag(&self) -> &'static str {
        match self {
            Self::Arbitrum => "arbitrum",
            Self::Base => "base",
        }
    }
}

/// Derive an L2's confirmed root, and the proof of it the enclave will recheck.
async fn get_l2_root(
    kind: L2Kind,
    l1: &crate::ethereum::ExecutionReader,
    l2: &crate::ethereum::ExecutionReader,
    anchor: &str,
    l1_block: u64,
    l1_state_root: alloy_primitives::B256,
) -> Result<(tee_node::origins::AttestedRoot, serde_json::Value)> {
    use tee_node::origins::ethereum_l2::{
        get_arbitrum_root, get_base_root, RollupLayout,
    };

    match kind {
        L2Kind::Arbitrum => {
            let proof = crate::ethereum_l2::get_arbitrum_root_proof(
                l1,
                l2,
                anchor.parse()?,
                RollupLayout::ARBITRUM_SEPOLIA,
                l1_block,
            )
            .await?;
            let root = get_arbitrum_root(l1_state_root, &proof)?;
            Ok((root, serde_json::to_value(proof)?))
        }
        L2Kind::Base => {
            let proof =
                crate::ethereum_l2::get_base_root_proof(l1, l2, anchor.parse()?, l1_block).await?;
            let root = get_base_root(l1_state_root, &proof)?;
            Ok((root, serde_json::to_value(proof)?))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn attest_l2(
    kind: L2Kind,
    beacon: &str,
    l1_execution: &str,
    l2_archive: &str,
    enclave_url: &str,
    trusted_state_hex: &str,
    anchor: &str,
    merkle_tree_hook: &str,
    mailbox: &str,
    base_slot: u64,
    out: Option<String>,
) -> Result<()> {
    use crate::enclave::EnclaveClient;
    use crate::ethereum::{expected_current_slot, EthereumReader, ExecutionReader};

    let trusted_raw = hex::decode(trusted_state_hex.trim_start_matches("0x"))?;
    let trusted = tee_attestation::decode_ism_state(&trusted_raw)?;

    let beacon_reader = EthereumReader::new(beacon);
    let config = beacon_reader.chain_config().await?;
    let (store, _checkpoint) =
        rebuild_ethereum_store(&beacon_reader, &config, &trusted, None).await?;

    let finality = beacon_reader.finality_update().await?;
    let slot = expected_current_slot(config.genesis_time);
    let l1_block = *finality
        .finalized_header()
        .execution()
        .map_err(|_| anyhow::anyhow!("finalized header has no execution payload"))?
        .block_number();

    let l1 = ExecutionReader::new(l1_execution);
    let l2 = ExecutionReader::new(l2_archive);

    let l1_state_root: alloy_primitives::B256 = l1
        .call(
            "eth_getBlockByNumber",
            serde_json::json!([format!("0x{l1_block:x}"), false]),
        )
        .await?["stateRoot"]
        .as_str()
        .context("L1 block has no state root")?
        .parse()?;

    // Which L2 block Ethereum has confirmed, proven rather than asked for. Deriving the root
    // here too is not redundant: it is how this knows which L2 block to read the tree at, and
    // a mismatch surfaces before minutes of proving rather than after.
    let (l2_root, root_proof) =
        get_l2_root(kind, &l1, &l2, anchor, l1_block, l1_state_root).await?;

    anyhow::ensure!(
        l2_root.height > trusted.height,
        "the confirmed L2 head {} has not passed the trusted height {}",
        l2_root.height,
        trusted.height
    );

    let hook: alloy_primitives::Address = merkle_tree_hook.parse()?;
    let mailbox_address: alloy_primitives::Address = mailbox.parse()?;

    let tree_proof = l2.merkle_tree_proof(hook, base_slot, l2_root.height).await?;
    let snapshot_proof = l2.merkle_tree_proof(hook, base_slot, trusted.height).await?;
    let snapshot = tee_node::hyperlane_state::get_evm_merkle_tree(
        alloy_primitives::B256::from(trusted.state_root),
        &snapshot_proof,
    )?;

    let dispatched = l2
        .dispatched_messages(mailbox_address, hook, trusted.height + 1, l2_root.height)
        .await?;
    println!(
        "confirmed L2 block {} | trusted {} | {} new leaves",
        l2_root.height,
        trusted.height,
        dispatched.len()
    );
    anyhow::ensure!(!dispatched.is_empty(), "nothing to attest");

    let mut tree_address = [0u8; 32];
    tree_address[12..].copy_from_slice(hook.as_slice());

    let mut tree_input = serde_json::to_value(&tree_proof)?;
    tree_input
        .as_object_mut()
        .context("tree proof must be an object")?
        .insert("kind".into(), serde_json::json!("evm"));

    let request = serde_json::json!({
        "trusted_state": hex::encode(&trusted_raw),
        "origin": {
            "chain": kind.chain_tag(),
            "ethereum": {
                "chain": "ethereum",
                "store": store,
                "updates": { "committee_updates": [], "finality_update": finality },
                "expected_current_slot": slot,
            },
            "proof": root_proof,
        },
        "tree": tree_input,
        "tree_snapshot": snapshot,
        "message_ids": dispatched.iter().map(|d| d.message_id).collect::<Vec<_>>(),
        "merkle_tree_address": tree_address,
    });

    let attestation = EnclaveClient::new(enclave_url).attest(&request).await?;
    println!("attested new state 0x{}", attestation.new_state);
    println!("messages           {}", attestation.message_ids.len());

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
        println!("wrote {path}");
    }
    Ok(())
}

/// Gather one Celestia step and hand it to the enclave.
pub async fn attest_celestia(
    rpc: &str,
    archive: Option<&str>,
    enclave_url: &str,
    trusted_state_hex: &str,
    merkle_tree_hook_hex: &str,
    lag: u64,
    out: Option<String>,
) -> Result<()> {
    use crate::celestia::CelestiaReader;
    use crate::enclave::EnclaveClient;

    let trusted_raw = hex::decode(trusted_state_hex.trim_start_matches("0x"))?;
    let trusted = tee_attestation::decode_ism_state(&trusted_raw)?;
    let hook_id: [u8; 32] = hex::decode(merkle_tree_hook_hex.trim_start_matches("0x"))?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("merkle tree hook must be 32 bytes"))?;

    let reader = CelestiaReader::new(rpc)?;
    // Everything at the trusted height is historical: the header, the state proof and the
    // tx search. Mocha's public RPCs prune all three.
    let history = CelestiaReader::new(archive.unwrap_or(rpc))?;
    let head = reader.latest_height().await?;
    let target = head.saturating_sub(lag);
    anyhow::ensure!(
        target > trusted.height + 1,
        "head has not advanced past the trusted state ({} vs {})",
        target,
        trusted.height
    );

    // The header we verify is one past the state it commits to.
    let new_block = reader.light_block(target + 1).await?;
    let trusted_block = history.light_block(trusted.height + 1).await?;

    let (hook_bytes, proof) = reader.merkle_tree_hook_proof(hook_id, target).await?;
    let onchain = tee_node::state_proofs::decode_merkle_tree_hook(&hook_bytes)?;

    // Everything inserted since the ISM's trusted height, in tree order.
    let inserted = history.dispatched_messages(trusted.height + 1, target).await?;
    println!("head {head} | attesting state at {target} | {} new leaves", inserted.len());
    anyhow::ensure!(!inserted.is_empty(), "nothing to attest; no messages dispatched");

    // The tree as it stood at the ISM's trusted height, proven rather than assumed. The
    // enclave replays the new leaves onto it and checks the result against the tree it just
    // proved at the head, so a wrong snapshot cannot pass.
    let (snapshot_bytes, _) = history
        .merkle_tree_hook_proof(hook_id, trusted.height)
        .await
        .context("reading the merkle tree at the trusted height; set `archive_rpc` if pruned")?;
    let snapshot = tee_node::state_proofs::decode_merkle_tree_hook(&snapshot_bytes)?;
    anyhow::ensure!(
        snapshot.count as usize + inserted.len() == onchain.count as usize,
        "snapshot has {} leaves and {} were found, but the head proves {}",
        snapshot.count,
        inserted.len(),
        onchain.count
    );

    let request = serde_json::json!({
        "trusted_state": hex::encode(&trusted_raw),
        "origin": {
            "chain": "celestia",
            "store": { "trusted": trusted_block },
            "updates": [new_block],
            // The coprocessor's clock. The enclave uses it only for the trusting-period
            // check and never derives it from the header being verified, which is what
            // keeps that check meaningful.
            "now": tendermint::Time::from_unix_timestamp(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs() as i64,
                0,
            )?,
        },
        "tree": {
            "kind": "celestia",
            "hook_id": hook_id,
            "hook_bytes": hook_bytes,
            "proof": proof,
        },
        "tree_snapshot": snapshot,
        "message_ids": inserted.iter().map(|m| m.message_id).collect::<Vec<_>>(),
        "merkle_tree_address": hook_id,
    });

    let client = EnclaveClient::new(enclave_url);
    let attestation = client.attest(&request).await?;

    println!("attested new state 0x{}", attestation.new_state);
    println!("quote bytes        {}", attestation.quote.len() / 2);
    println!("messages           {}", attestation.message_ids.len());
    if let Some(path) = out {
        let record = serde_json::json!({
            "attestation": {
                "quote": attestation.quote,
                "event_log": attestation.event_log,
                "payload": attestation.payload,
                "new_state": attestation.new_state,
            },
            "messages": inserted.iter().map(|m| hex::encode(&m.message)).collect::<Vec<_>>(),
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&record)?)?;
        println!("wrote {path}");
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
    use helios_consensus_core::{apply_bootstrap, verify_bootstrap};
    use crate::ethereum::EthereumReader;
    use tee_node::origins::ethereum::{commit_ethereum_store, get_ethereum_root, EthereumStore};

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
    let head = get_ethereum_root(&store)?;

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
    println!("genesis state    0x{}", hex::encode(tee_attestation::encode_ism_state(&state)));
    Ok(())
}

/// Produce the two Groth16 proofs the destination needs.
pub async fn prove(attestation_path: &str, elf_dir: &str, out: &str) -> Result<()> {
    use sp1_sdk::{Prover, ProverClient, SP1Stdin};
    use std::time::Instant;
    use crate::enclave::fetch_collateral;

    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(attestation_path)?)?;
    let att = &record["attestation"];
    let quote_hex = att["quote"].as_str().context("quote")?;

    println!("fetching Intel collateral from PCCS...");
    let collateral = fetch_collateral(quote_hex).await?;

    let payload = hex::decode(att["payload"].as_str().context("payload")?)?;
    let update = tee_attestation::decode_attested_update(&payload)?;

    // The prover's clock, which the circuit bounds to the attested head's timestamp. It may
    // not be freely chosen: too far back and a revoked TCB could be revived, too far forward
    // and the collateral has not been issued yet.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    println!("attested head {} | prover clock {now}", update.new_state.timestamp);

    let inputs = tee_attestation::AttestationInputs {
        quote: hex::decode(quote_hex.trim_start_matches("0x"))?,
        event_log: att["event_log"].as_str().context("event log")?.as_bytes().to_vec(),
        collateral: tee_attestation::AttestationInputs::encode_collateral(&collateral),
        now,
        payload: payload.clone(),
    };

    let mut stdin = SP1Stdin::new();
    stdin.write(&inputs);

    let client = ProverClient::builder().cpu().build();
    let mut proofs = serde_json::Map::new();
    for (name, elf_name) in
        [("state_transition", "tee-state-transition"), ("state_membership", "tee-state-membership")]
    {
        let elf = std::fs::read(std::path::Path::new(elf_dir).join(elf_name))
            .with_context(|| format!("{elf_name}: run `circuit-tool build` in tee-circuit"))?;
        let (pk, vk) = client.setup(&elf);
        println!("proving {name}...");
        let started = Instant::now();
        let proof = client.prove(&pk, &stdin).groth16().run()?;
        client.verify(&proof, &vk)?;
        println!("  {name}: {:.0}s, {} proof bytes", started.elapsed().as_secs_f64(), proof.bytes().len());

        proofs.insert(
            name.to_string(),
            serde_json::json!({
                "proof": hex::encode(proof.bytes()),
                "public_values": hex::encode(proof.public_values.as_slice()),
            }),
        );
    }

    let mut record = record.clone();
    record["proofs"] = serde_json::Value::Object(proofs);

    // The same file is what the UI reads, so record the batch in the shape it expects:
    // which messages were attested together, and the measurements of the enclave that did
    // it. Everything here is public.
    let measurements = enclave_measurements(att)?;
    record["height"] = update.new_state.height.into();
    record["state_root"] = format!("0x{}", hex::encode(update.new_state.state_root)).into();
    record["quote"] = quote_hex.into();
    record["measurements"] = serde_json::to_value(measurements)?;
    record["batch"] = update
        .message_ids
        .iter()
        .map(|id| format!("0x{}", hex::encode(id)))
        .collect::<Vec<_>>()
        .into();

    std::fs::write(out, serde_json::to_vec_pretty(&record)?)?;
    println!("wrote {out}");
    Ok(())
}

/// Pull the measurements out of the quote and event log, for display.
pub fn enclave_measurements(
    attestation: &serde_json::Value,
) -> Result<crate::api::Measurements> {
    let quote_bytes =
        hex::decode(attestation["quote"].as_str().context("quote")?.trim_start_matches("0x"))?;
    let quote = dcap_qvl::quote::Quote::parse(&quote_bytes)
        .map_err(|e| anyhow::anyhow!("quote does not parse: {e:?}"))?;
    let td = quote.report.as_td10().context("not a TDX quote")?;

    let events: Vec<tee_attestation::EventLog> =
        serde_json::from_str(attestation["event_log"].as_str().context("event log")?)?;
    let read = |name: &str| {
        tee_attestation::get_event_value(&events, name)
            .map(hex::encode)
            .unwrap_or_default()
    };

    Ok(crate::api::Measurements {
        mr_td: hex::encode(td.mr_td),
        os_image_hash: read("os-image-hash"),
        compose_hash: read("compose-hash"),
    })
}

pub async fn run(config: Config) -> Result<()> {
    let tick = Duration::from_secs(config.tick_secs);
    let cpu = cpu_prover_permit();
    let store = std::sync::Arc::new(ProofStore::new(expand_home(&config.proof_dir)));

    let mut routes = Vec::new();
    for route in config.routes {
        info!(
            route = %route.name,
            origin = route.origin.domain(),
            destination = route.destination.domain(),
            "configured"
        );
        routes.push(tokio::spawn(run_route(route, store.clone(), cpu.clone(), tick)));
    }
    info!(tick_secs = config.tick_secs, routes = routes.len(), "coprocessor running");

    // A route runs until the process stops. If one panics, take the service down so systemd
    // restarts it, rather than leaving a direction silently dead.
    for handle in routes {
        handle.await?;
    }
    Ok(())
}

pub fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}
