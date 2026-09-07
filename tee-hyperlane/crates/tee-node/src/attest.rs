//! The one thing the enclave does: verify, attest, forget.
//!
//! A request carries everything needed and nothing secret. The enclave checks the supplied
//! light-client store against the commitment in the ISM state it was given, walks the store
//! forward, derives the origin state root, proves the origin Hyperlane tree under that root,
//! confirms the claimed message batch is exactly the new leaves, and asks dstack to sign the
//! result into a TDX quote.
//!
//! Nothing is written to disk and nothing is kept between requests: the destination chain's
//! ISM state *is* the light client's database.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tee_attestation::{
    encode_attested_update, hash_attested_update, AttestedUpdate, IsmState,
};

use crate::hyperlane_state::{
    get_evm_merkle_tree, verify_message_batch, EvmTreeProof, HyperlaneStateError,
};
use crate::origins::celestia::{
    self, get_celestia_root, verify_celestia_updates, CelestiaError, CelestiaStore,
};
use crate::origins::ethereum::{
    self, get_ethereum_root, verify_ethereum_updates, EthereumError, EthereumStore,
    EthereumUpdates,
};
use crate::origins::ethereum_l2::{
    get_arbitrum_root, get_base_root, ArbitrumError, ArbitrumRootProof, BaseError, BaseRootProof,
};
use crate::origins::{AttestedRoot, Origin};
use crate::state_proofs::{
    get_celestia_merkle_tree, CelestiaStateError, Ics23TreeProof,
};
use hyperlane_types::MerkleTree;

/// How the enclave is asked to advance one ISM by one step.
#[derive(Serialize, Deserialize)]
pub struct AttestRequest {
    /// The ISM's current state, read from the destination chain, hex-encoded.
    #[serde(with = "hex_ism_state")]
    pub trusted_state: IsmState,
    pub origin: OriginInput,
    /// Where the origin's Hyperlane tree lives, and the proof of its current contents.
    pub tree: TreeInput,
    /// The tree as of `trusted_state`, replayed onto rather than trusted.
    pub tree_snapshot: MerkleTree,
    /// Message ids claimed to be the new leaves, in insert order.
    pub message_ids: Vec<[u8; 32]>,
    /// Origin merkle tree hook, as the ISM records it.
    pub merkle_tree_address: [u8; 32],
}

mod hex_ism_state {
    use serde::{Deserialize, Deserializer, Serializer};
    use tee_attestation::{decode_ism_state, encode_ism_state, IsmState};

    pub fn serialize<S: Serializer>(v: &IsmState, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(encode_ism_state(v)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<IsmState, D::Error> {
        let text = String::deserialize(d)?;
        let raw = hex::decode(text.strip_prefix("0x").unwrap_or(&text))
            .map_err(serde::de::Error::custom)?;
        decode_ism_state(&raw).map_err(serde::de::Error::custom)
    }
}

/// Per-origin inputs. Adding a network adds one variant and one arm.
#[derive(Serialize, Deserialize)]
#[serde(tag = "chain", rename_all = "snake_case")]
pub enum OriginInput {
    Ethereum {
        store: EthereumStore,
        updates: EthereumUpdates,
        expected_current_slot: u64,
    },
    Celestia {
        store: CelestiaStore,
        updates: Vec<tendermint_light_client_verifier::types::LightBlock>,
        now: tendermint::Time,
    },
    /// Arbitrum and Base ride on Ethereum's light client rather than their own.
    Arbitrum {
        ethereum: Box<OriginInput>,
        proof: ArbitrumRootProof,
    },
    Base {
        ethereum: Box<OriginInput>,
        proof: BaseRootProof,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TreeInput {
    Evm(EvmTreeProof),
    Celestia { hook_id: [u8; 32], hook_bytes: Vec<u8>, proof: Ics23TreeProof },
}

#[derive(Debug, thiserror::Error)]
pub enum AttestError {
    #[error("supplied light-client store does not match the commitment in the ISM state")]
    StoreCommitmentMismatch,
    #[error("origin domain {got} does not match the ISM's {expected}")]
    WrongOrigin { got: u32, expected: u32 },
    #[error(transparent)]
    Ethereum(#[from] EthereumError),
    #[error(transparent)]
    Celestia(#[from] CelestiaError),
    #[error(transparent)]
    Arbitrum(#[from] ArbitrumError),
    #[error(transparent)]
    Base(#[from] BaseError),
    #[error(transparent)]
    HyperlaneState(#[from] HyperlaneStateError),
    #[error(transparent)]
    CelestiaState(#[from] CelestiaStateError),
    #[error("an L2 origin must be derived from an Ethereum light client")]
    L2NeedsEthereum,
}

/// Verify everything in the request and produce the update to be attested.
///
/// Returns the payload as well, because the caller hashes it into `report_data`.
pub fn build_attested_update(
    request: &mut AttestRequest,
) -> Result<(AttestedUpdate, Vec<u8>), AttestError> {
    let expected = request.trusted_state.origin_domain;

    let (root, store_commit) = advance_origin(&mut request.origin, &request.trusted_state)?;

    let origin = origin_of(&request.origin);
    if origin.domain() != expected {
        return Err(AttestError::WrongOrigin { got: origin.domain(), expected });
    }

    let onchain_tree = match &request.tree {
        TreeInput::Evm(proof) => get_evm_merkle_tree(root.state_root, proof)?,
        TreeInput::Celestia { hook_id, hook_bytes, proof } => {
            get_celestia_merkle_tree(root.state_root.0, *hook_id, hook_bytes, proof)?
        }
    };
    verify_message_batch(request.tree_snapshot, &request.message_ids, &onchain_tree)?;

    let new_state = IsmState {
        state_root: root.state_root.0,
        origin_domain: expected,
        height: root.height,
        timestamp: root.timestamp,
        lc_store_commit: store_commit,
        identity_digest: request.trusted_state.identity_digest,
    };
    let update = AttestedUpdate {
        prev_state: request.trusted_state,
        new_state,
        merkle_tree_address: request.merkle_tree_address,
        message_ids: request.message_ids.clone(),
    };
    let payload = encode_attested_update(&update);
    Ok((update, payload))
}

/// The 32 bytes the enclave asks dstack to sign into the quote.
pub fn report_data_for(update: &AttestedUpdate) -> [u8; 32] {
    hash_attested_update(update)
}

fn origin_of(input: &OriginInput) -> Origin {
    match input {
        OriginInput::Ethereum { .. } => Origin::Ethereum,
        OriginInput::Celestia { .. } => Origin::Celestia,
        OriginInput::Arbitrum { .. } => Origin::Arbitrum,
        OriginInput::Base { .. } => Origin::Base,
    }
}

/// Walk one origin's light client forward and return its root plus the new store commitment.
fn advance_origin(
    input: &mut OriginInput,
    trusted: &IsmState,
) -> Result<(AttestedRoot, [u8; 32]), AttestError> {
    match input {
        OriginInput::Ethereum { store, updates, expected_current_slot } => {
            if ethereum::commit_ethereum_store(store) != trusted.lc_store_commit {
                return Err(AttestError::StoreCommitmentMismatch);
            }
            verify_ethereum_updates(store, updates, *expected_current_slot)?;
            Ok((get_ethereum_root(store)?, ethereum::commit_ethereum_store(store)))
        }
        OriginInput::Celestia { store, updates, now } => {
            if celestia::commit_celestia_store(store) != trusted.lc_store_commit {
                return Err(AttestError::StoreCommitmentMismatch);
            }
            verify_celestia_updates(store, updates, *now)?;
            Ok((get_celestia_root(store)?, celestia::commit_celestia_store(store)))
        }
        // An L2's trust chain starts at Ethereum: verify L1 first, then read the L2 root out
        // of L1 storage. The commitment carried in the ISM state is Ethereum's, because
        // Ethereum's light client is the thing with state worth remembering.
        OriginInput::Arbitrum { ethereum, proof } => {
            let (l1, commit) = advance_ethereum(ethereum, trusted)?;
            Ok((get_arbitrum_root(l1.state_root, proof)?, commit))
        }
        OriginInput::Base { ethereum, proof } => {
            let (l1, commit) = advance_ethereum(ethereum, trusted)?;
            Ok((get_base_root(l1.state_root, proof)?, commit))
        }
    }
}

fn advance_ethereum(
    input: &mut OriginInput,
    trusted: &IsmState,
) -> Result<(AttestedRoot, [u8; 32]), AttestError> {
    match input {
        OriginInput::Ethereum { .. } => advance_origin(input, trusted),
        _ => Err(AttestError::L2NeedsEthereum),
    }
}

/// Commit to a light-client store plus the tree snapshot, for callers that want one value.
pub fn commit_bridge_state(store_commit: [u8; 32], tree: &MerkleTree) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"tee-isms/bridge-state/v1");
    h.update(store_commit);
    h.update(tree.count.to_be_bytes());
    for node in &tree.branch {
        h.update(node);
    }
    h.finalize().into()
}

/// dstack's quote response, as its unix socket returns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteResponse {
    /// Hex-encoded TDX quote.
    pub quote: String,
    /// The runtime event log, as JSON.
    pub event_log: String,
}
