//! Celestia as an origin: a tendermint light client and an ics23 proof of the hook.

use anyhow::{Context, Result};
use tracing::{debug, info};

/// Anchor a Celestia-origin ISM to a live header.
pub async fn bootstrap_celestia(
    rpc: &str,
    lag: u64,
    height: Option<u64>,
    identity_digest: &str,
) -> Result<()> {
    use crate::celestia::CelestiaReader;
    use tee_node::origins::celestia::{celestia_root, commit_celestia_store, CelestiaStore};

    let reader = CelestiaReader::new(rpc)?;
    let anchor = match height {
        Some(h) => h,
        None => reader.latest_height().await?.saturating_sub(lag).max(2),
    };
    let trusted = reader.light_block(anchor).await?;
    let store = CelestiaStore { trusted };
    let root = celestia_root(&store)?;

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
    println!(
        "genesis state    0x{}",
        hex::encode(tee_attestation::encode_ism_state(&state))
    );
    Ok(())
}

/// Gather one Celestia step and hand it to the enclave.
pub async fn attest_celestia(
    rpc: &str,
    archive: Option<&str>,
    enclave_url: &str,
    trusted_state_hex: &str,
    destination_domain: u32,
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
    super::record_attestable_head(out.as_deref(), target);
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

    // The tree as it stood at the ISM's trusted height, proven rather than assumed. The
    // enclave replays the new leaves onto it and checks the result against the tree proved at
    // the head, so a wrong snapshot cannot pass.
    let (snapshot_bytes, snapshot_proof) = history
        .merkle_tree_hook_proof(hook_id, trusted.height)
        .await
        .context("reading the merkle tree at the trusted height; set `archive_rpc` if pruned")?;
    let snapshot = tee_node::state_proofs::decode_merkle_tree_hook(&snapshot_bytes)?;

    // Two counts answer "was anything dispatched" exactly, before any transaction search.
    // The search is proportional to how far behind the route is; this is two proofs whatever
    // the gap. An idle route therefore costs nothing to keep current, which is what stopped
    // being true when routes began skipping batches that were not addressed to them.
    let expected = onchain.count.saturating_sub(snapshot.count) as usize;
    if expected == 0 {
        super::record_scanned(out.as_deref(), target);
        anyhow::bail!("nothing to attest; the origin tree has not grown");
    }

    // Everything inserted since the ISM's trusted height, in tree order.
    let inserted = history
        .dispatched_messages(trusted.height + 1, target)
        .await?;
    // The tree says how many there should be, so a search that quietly returns too few is
    // caught here rather than by the enclave rejecting the replay an hour later.
    anyhow::ensure!(
        inserted.len() == expected,
        "the tree grew by {expected} leaves between {} and {target} but the search found {}; \
         the origin RPC is missing transactions",
        trusted.height,
        inserted.len()
    );
    info!(
        head,
        height = target,
        leaves = inserted.len(),
        "attesting celestia"
    );
    anyhow::ensure!(
        !inserted.is_empty(),
        "nothing to attest; no messages dispatched"
    );
    // Every Celestia dispatch lands in this one tree, so "something was sent" is not the same
    // question as "something was sent here". Proving on the former burned two extra proofs
    // per transfer.
    let ours: Vec<Vec<u8>> = inserted.iter().map(|m| m.message.clone()).collect();
    if !super::any_for_destination(&ours, destination_domain)
        && !super::heartbeat_due(out.as_deref())
    {
        anyhow::bail!("nothing to attest; no messages for domain {destination_domain}");
    }

    anyhow::ensure!(
        snapshot.count as usize + inserted.len() == onchain.count as usize,
        "snapshot has {} leaves and {} were found, but the head proves {}",
        snapshot.count,
        inserted.len(),
        onchain.count
    );
    let snapshot_input = serde_json::json!({
        "kind": "celestia",
        "hook_id": hook_id,
        "hook_bytes": snapshot_bytes,
        "proof": snapshot_proof,
    });

    let request = serde_json::json!({
        "protocol": tee_node::attest::PROTOCOL_VERSION,
        "trusted_state": hex::encode(&trusted_raw),
        "origin": {
            "chain": "celestia",
            "store": { "trusted": trusted_block },
            "updates": [new_block],
        },
        "tree": {
            "kind": "celestia",
            "hook_id": hook_id,
            "hook_bytes": hook_bytes,
            "proof": proof,
        },
        "tree_snapshot": snapshot_input,
        "message_ids": inserted.iter().map(|m| m.message_id).collect::<Vec<_>>(),
        "merkle_tree_address": hook_id,
    });

    let client = EnclaveClient::new(enclave_url);
    let attestation = client.attest(&request).await?;

    info!(messages = attestation.message_ids.len(), "enclave attested");
    super::record_advanced(out.as_deref());
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
        debug!(path, "wrote attestation");
    }
    Ok(())
}
