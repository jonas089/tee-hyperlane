//! Arbitrum and Base: Ethereum's flow, plus the L1 storage proof that says which L2 block
//! Ethereum has confirmed.
//!
//! The ISM state carries *Ethereum's* light-client commitment, because Ethereum's client is
//! the thing with state worth remembering. What is L2-specific is only where the confirmed
//! root is read from, and that address is pinned in the enclave rather than sent to it.

use anyhow::{Context, Result};
use tracing::{debug, info};

use super::{bootstrap_store, evm_tree_input, rebuild_ethereum_store};

/// Which rollup an L2 origin is, and what it needs to name its anchor contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L2Kind {
    Arbitrum,
    Base,
}

impl std::str::FromStr for L2Kind {
    type Err = anyhow::Error;

    fn from_str(name: &str) -> Result<Self> {
        match name {
            "arbitrum" => Ok(Self::Arbitrum),
            "base" => Ok(Self::Base),
            other => anyhow::bail!("unknown L2 `{other}`; expected arbitrum or base"),
        }
    }
}

impl L2Kind {
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
    use tee_node::origins::ethereum_l2::{verify_arbitrum_root, verify_base_root, RollupLayout};

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
            let root = verify_arbitrum_root(l1_state_root, &proof)?;
            Ok((root, serde_json::to_value(proof)?))
        }
        L2Kind::Base => {
            let proof =
                crate::ethereum_l2::get_base_root_proof(l1, l2, anchor.parse()?, l1_block).await?;
            let root = verify_base_root(l1_state_root, &proof)?;
            Ok((root, serde_json::to_value(proof)?))
        }
    }
}

#[allow(clippy::too_many_arguments)]

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
    use tee_node::origins::ethereum::{commit_ethereum_store, ethereum_root};

    let beacon_reader = EthereumReader::new(beacon);
    let config = beacon_reader.chain_config().await?;
    let checkpoint = match checkpoint {
        Some(explicit) => explicit,
        None => beacon_reader.finalized_root().await?,
    };
    let store = bootstrap_store(&beacon_reader, &config, &checkpoint).await?;

    let l1_block = ethereum_root(&store)?.height;
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
    println!(
        "genesis state      0x{}",
        hex::encode(tee_attestation::encode_ism_state(&state))
    );
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

/// Gather one L2 step and hand it to the enclave.
///
/// `checkpoint` is not optional in practice. An L2-origin ISM cannot derive which L1
/// checkpoint its store was built from, because the trusted timestamp is the *L2's* and says
/// nothing about L1, so `rebuild_ethereum_store` falls back to walking the last eight
/// finalized epochs. That covers roughly fifty minutes: a route bootstrapped longer ago than
/// that silently stops resuming, and the error it raises tells you to set the very field this
/// argument carries. Passing `None` here is what made that advice impossible to follow.
pub async fn attest_l2(
    kind: L2Kind,
    beacon: &str,
    l1_execution: &str,
    l2_archive: &str,
    logs_rpc: Option<&str>,
    enclave_url: &str,
    trusted_state_hex: &str,
    anchor: &str,
    merkle_tree_hook: &str,
    mailbox: &str,
    base_slot: u64,
    checkpoint: Option<&str>,
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
    let l1_block = *finality
        .finalized_header()
        .execution()
        .map_err(|_| anyhow::anyhow!("finalized header has no execution payload"))?
        .block_number();

    let l1 = ExecutionReader::new(l1_execution);
    let l2 = ExecutionReader::new(l2_archive).with_logs_rpc(logs_rpc);

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

    super::record_attestable_head(out.as_deref(), l2_root.height);
    anyhow::ensure!(
        l2_root.height > trusted.height,
        "the confirmed L2 head {} has not passed the trusted height {}",
        l2_root.height,
        trusted.height
    );

    let hook: alloy_primitives::Address = merkle_tree_hook.parse()?;
    let mailbox_address: alloy_primitives::Address = mailbox.parse()?;

    let tree_proof = l2
        .merkle_tree_proof(hook, base_slot, l2_root.height)
        .await?;
    let snapshot_proof = l2
        .merkle_tree_proof(hook, base_slot, trusted.height)
        .await
        .context("reading the merkle tree at the trusted height; set `archive_rpc` if pruned")?;

    let dispatched = l2
        .dispatched_messages(mailbox_address, hook, trusted.height + 1, l2_root.height)
        .await?;
    info!(
        rollup = kind.chain_tag(),
        height = l2_root.height,
        trusted = trusted.height,
        l1_block,
        leaves = dispatched.len(),
        "attesting l2"
    );
    anyhow::ensure!(!dispatched.is_empty(), "nothing to attest");

    let mut tree_address = [0u8; 32];
    tree_address[12..].copy_from_slice(hook.as_slice());

    let tree_input = evm_tree_input(&tree_proof)?;
    let snapshot_input = evm_tree_input(&snapshot_proof)?;

    let request = serde_json::json!({
        "protocol": tee_node::attest::PROTOCOL_VERSION,
        "trusted_state": hex::encode(&trusted_raw),
        "origin": {
            "chain": kind.chain_tag(),
            "ethereum": {
                "chain": "ethereum",
                "store": store,
                "updates": { "committee_updates": [], "finality_update": finality },
            },
            "proof": root_proof,
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
