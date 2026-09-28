//! The one thing the enclave does: verify, attest, forget.
//!
//! The same steps for every origin, which is the point of `origin::Origin`:
//!
//! 1. verify the head the request describes, from the state the ISM already trusts
//! 2. read the Hyperlane tree at both ends: under the trusted root, and under the new one
//! 3. check both trees were read at the address the ISM pins
//! 4. check the claimed ids are exactly the leaves between the two trees
//! 5. hand back the update for dstack to sign
//!
//! Nothing is written to disk and nothing is kept between requests: the destination chain's
//! ISM state *is* the light client's database.

use crate::state::{AttestedUpdate, IsmState};
use serde::Deserialize;
use serde_json::Value;

use crate::origin;
use hyperlane_types::{get_tree_root, insert_leaf, MerkleTree};

/// The request shape this enclave understands.
///
/// Bumped whenever a field is added, removed, or stops being honoured, so a caller built for
/// an older shape is refused rather than having fields silently ignored.
///
/// 1 is the shape since the per-chain traits: `chain` at the top level, its `input` and both
/// tree proofs left to it. Numbering restarted there; no enclave speaking an earlier shape runs.
pub const PROTOCOL_VERSION: u32 = 1;

/// How the enclave is asked to advance one ISM by one step.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttestRequest {
    /// Must equal `PROTOCOL_VERSION`.
    pub protocol: u32,
    /// The ISM's current state, read from the destination chain, hex-encoded.
    #[serde(with = "hex_ism_state")]
    pub trusted_state: IsmState,
    /// Which origin, as `origin::Chain::name`.
    pub chain: String,
    /// Whatever that origin needs to verify its head. Only the origin parses it.
    pub input: Value,
    /// Proof of the origin's Hyperlane tree at the new head.
    pub tree: Value,
    /// Proof of the same tree as of `trusted_state`, read under that state's own root.
    pub tree_snapshot: Value,
    /// Message ids claimed to be the new leaves, in insert order.
    pub message_ids: Vec<[u8; 32]>,
    /// The origin merkle tree hook the ISM pins.
    pub merkle_tree_address: [u8; 32],
}

mod hex_ism_state {
    use crate::state::IsmState;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<IsmState, D::Error> {
        let text = String::deserialize(d)?;
        let raw = hex::decode(text.strip_prefix("0x").unwrap_or(&text))
            .map_err(serde::de::Error::custom)?;
        IsmState::decode(&raw).map_err(serde::de::Error::custom)
    }
}

/// Verify everything in the request and produce the update to be attested.
///
/// Returns the payload as well, because the caller hashes it into `report_data`.
pub fn build_attested_update(request: AttestRequest) -> anyhow::Result<(AttestedUpdate, Vec<u8>)> {
    let chain = origin::find(&request.chain)
        .ok_or_else(|| anyhow::anyhow!("this enclave does not attest `{}`", request.chain))?;
    attest_with(chain, request)
}

/// Everything after the registry lookup, for one known chain. Separate so the pipeline can be
/// tested against a stand-in chain without light-client fixtures.
pub fn attest_with(
    chain: &origin::Chain,
    request: AttestRequest,
) -> anyhow::Result<(AttestedUpdate, Vec<u8>)> {
    anyhow::ensure!(
        request.protocol == PROTOCOL_VERSION,
        "request protocol {}, but this enclave speaks {PROTOCOL_VERSION}",
        request.protocol
    );
    let trusted = request.trusted_state;
    anyhow::ensure!(
        chain.domain == trusted.origin_domain,
        "{} is domain {}, but the ISM is for domain {}",
        chain.name,
        chain.domain,
        trusted.origin_domain
    );

    let head = chain.origin.verify(request.input, &trusted)?;

    // Both ends of the replay are read from state the ISM already trusts: the snapshot under
    // the root the ISM last accepted, the head under the root this update moves it to. Taking
    // the snapshot on the caller's word would let it pick any earlier tree - every one is a
    // well-formed snapshot of public leaves - and skip the messages in between for good.
    let snapshot = chain
        .origin
        .merkle_tree(request.tree_snapshot, trusted.state_root.into())?;
    let at_head = chain.origin.merkle_tree(request.tree, head.root)?;

    // Proving a tree is not enough: anyone can deploy a hook, fill it with ids of their
    // choosing and prove it honestly under the real root. Only the address the ISM pins counts.
    for tree in [&snapshot, &at_head] {
        anyhow::ensure!(
            tree.address == request.merkle_tree_address,
            "tree proven at 0x{} but 0x{} was attested",
            hex::encode(tree.address),
            hex::encode(request.merkle_tree_address)
        );
    }
    verify_message_batch(snapshot.tree, &request.message_ids, &at_head.tree)?;

    let new_state = IsmState {
        state_root: head.root.0,
        origin_domain: trusted.origin_domain,
        height: head.height,
        timestamp: head.timestamp,
        lc_store_commit: head.store_commit,
        identity_digest: trusted.identity_digest,
    };
    let update = AttestedUpdate {
        prev_state: trusted,
        new_state,
        merkle_tree_address: request.merkle_tree_address,
        attested_at: head.attested_at,
        message_ids: request.message_ids,
    };
    let payload = update.encode();
    Ok((update, payload))
}

// ---------------------------------------------------------------- the batch check
//
// The one check every origin shares: the claimed ids are exactly the leaves between the tree
// the ISM trusts and the tree at the new head. An on-chain inclusion proof under a TEE-attested
// root would add nothing - whoever can forge the root can forge a proof under it - so what
// matters is that this code is inside the measurement, which it is.

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BatchError {
    #[error("a batch must carry at least one message id")]
    EmptyBatch,
    #[error("replaying {ids} message ids gave root {replayed} at count {count}, but the chain proves {onchain}")]
    ReplayMismatch {
        ids: usize,
        count: u32,
        replayed: String,
        onchain: String,
    },
    #[error("replayed {replayed} leaves but the chain proves {onchain}")]
    CountMismatch { replayed: u32, onchain: u32 },
    #[error("snapshot count {snapshot} exceeds the proven on-chain count {onchain}")]
    SnapshotAhead { snapshot: u32, onchain: u32 },
    #[error("merkle tree is full")]
    TreeFull,
}

/// Confirm that `message_ids` are exactly the leaves between the snapshot and the tree proven
/// out of chain state.
///
/// This checks the span, not where it starts: an incremental branch is a function of every
/// leaf ever inserted, so no snapshot beside the real one can reproduce the on-chain root, but
/// a snapshot further *along* it reproduces one perfectly. Both trees must therefore be read
/// from state the ISM already trusts, which `attest::build_attested_update` is what does.
pub fn verify_message_batch(
    snapshot: MerkleTree,
    message_ids: &[[u8; 32]],
    onchain: &MerkleTree,
) -> Result<(), BatchError> {
    // The destination allows one batch per state root, so attesting nothing still spends that
    // root's only slot. Nothing to attest is not worth an attestation.
    if message_ids.is_empty() {
        return Err(BatchError::EmptyBatch);
    }
    if snapshot.count > onchain.count {
        return Err(BatchError::SnapshotAhead {
            snapshot: snapshot.count,
            onchain: onchain.count,
        });
    }
    let mut replayed = snapshot;
    for id in message_ids {
        insert_leaf(&mut replayed, *id).map_err(|_| BatchError::TreeFull)?;
    }

    // Compare what the tree *means* - its leaf count and its root - rather than the raw
    // branch array. The two Hyperlane implementations disagree about unused levels:
    // hyperlane-cosmos pre-fills them with the canonical zero hashes, Solidity leaves them
    // zero. Those levels are above the highest set bit of `count`, so they contribute
    // nothing to the root and nothing to whether a message is in the tree. Comparing them
    // would make every Celestia-origin batch fail while proving nothing.
    if replayed.count != onchain.count {
        return Err(BatchError::CountMismatch {
            replayed: replayed.count,
            onchain: onchain.count,
        });
    }
    let got = get_tree_root(&replayed);
    let want = get_tree_root(onchain);
    if got != want {
        return Err(BatchError::ReplayMismatch {
            ids: message_ids.len(),
            count: replayed.count,
            replayed: hex::encode(got),
            onchain: hex::encode(want),
        });
    }
    Ok(())
}
