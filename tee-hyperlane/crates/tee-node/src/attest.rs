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

/// The request shape this enclave understands.
///
/// Bumped whenever a field is added, removed, or stops being honoured. Several fields have
/// been taken out because the caller should never have chosen them - the tree's base slot,
/// the L2 anchor and its layout, the clock - and a caller still sending those would otherwise
/// have them silently ignored, which looks like working. Requiring the version turns that
/// into a refusal.
///
/// 3 turns `tree_snapshot` from a decoded tree into a proof of one.
pub const PROTOCOL_VERSION: u32 = 3;

/// How the enclave is asked to advance one ISM by one step.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttestRequest {
    /// Must equal `PROTOCOL_VERSION`.
    pub protocol: u32,
    /// The ISM's current state, read from the destination chain, hex-encoded.
    #[serde(with = "hex_ism_state")]
    pub trusted_state: IsmState,
    pub origin: OriginInput,
    /// Where the origin's Hyperlane tree lives, and the proof of its current contents.
    pub tree: TreeInput,
    /// Proof of the same tree as of `trusted_state`, read under that state's own root.
    pub tree_snapshot: TreeInput,
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
///
/// `deny_unknown_fields` is deliberately absent: serde ignores it on internally tagged enums,
/// so writing it here would look like a guard and be none. `AttestRequest::protocol` is what
/// actually catches a stale caller.
#[derive(Serialize, Deserialize)]
#[serde(tag = "chain", rename_all = "snake_case")]
pub enum OriginInput {
    Ethereum {
        store: EthereumStore,
        updates: EthereumUpdates,
    },
    Celestia {
        store: CelestiaStore,
        updates: Vec<tendermint_light_client_verifier::types::LightBlock>,
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
    #[error("tree proven at 0x{proven} but 0x{attested} was attested")]
    WrongMerkleTree { proven: String, attested: String },
    #[error("the enclave has no usable clock")]
    NoClock,
    #[error("no merkle tree layout is pinned for origin domain {domain}")]
    NoTreeLayout { domain: u32 },
    #[error("request protocol {got}, but this enclave speaks {expected}")]
    WrongProtocol { got: u32, expected: u32 },
}

/// Verify everything in the request and produce the update to be attested.
///
/// Returns the payload as well, because the caller hashes it into `report_data`.
pub fn build_attested_update(
    request: &mut AttestRequest,
) -> Result<(AttestedUpdate, Vec<u8>), AttestError> {
    if request.protocol != PROTOCOL_VERSION {
        return Err(AttestError::WrongProtocol {
            got: request.protocol,
            expected: PROTOCOL_VERSION,
        });
    }
    let expected = request.trusted_state.origin_domain;

    // Where a tree was read must be the address being attested. All three are supplied by
    // the caller, and proving a tree is not enough on its own: anyone can deploy a merkle
    // tree hook, fill it with ids of their choosing, and prove it honestly under the real
    // state root. Only tying the proven address to the attested one makes that useless,
    // because the destination ISM pins the address it will accept.
    for tree in [&request.tree, &request.tree_snapshot] {
        let proven_at = tree_address_of(tree);
        if proven_at != request.merkle_tree_address {
            return Err(AttestError::WrongMerkleTree {
                proven: hex::encode(proven_at),
                attested: hex::encode(request.merkle_tree_address),
            });
        }
    }

    let (root, store_commit, attested_at) =
        advance_origin(&mut request.origin, &request.trusted_state)?;

    let origin = origin_of(&request.origin);
    if origin.domain() != expected {
        return Err(AttestError::WrongOrigin { got: origin.domain(), expected });
    }

    // Both ends of the replay are read from state the ISM already trusts: the snapshot under
    // the root the ISM last accepted, the head under the root this update is about to move
    // it to. Taking the snapshot on the caller's word instead would let it be chosen, and a
    // Hyperlane tree is incremental, so every intermediate tree the origin ever held is a
    // well-formed snapshot whose leaves are all public. A caller free to pick one picks the
    // head minus one leaf, replays a single id, reproduces the head's count and root exactly,
    // and every message in between is skipped for good - the root advances, that root's one
    // batch slot is spent, and those ids are never attested by any later batch either. That
    // is targeted censorship of one transfer with the bridge still looking healthy. Anchoring
    // the snapshot to `prev_state` removes the choice: the replay now spans exactly the
    // distance the ISM is moving.
    let snapshot =
        read_tree(&request.tree_snapshot, request.trusted_state.state_root, expected)?;
    let onchain_tree = read_tree(&request.tree, root.state_root.0, expected)?;
    verify_message_batch(snapshot, &request.message_ids, &onchain_tree)?;

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
        attested_at,
        message_ids: request.message_ids.clone(),
    };
    let payload = encode_attested_update(&update);
    Ok((update, payload))
}

/// The 32 bytes the enclave asks dstack to sign into the quote.
pub fn report_data_for(update: &AttestedUpdate) -> [u8; 32] {
    hash_attested_update(update)
}

/// Read a Hyperlane merkle tree out of a proof, under the state root it must verify against.
fn read_tree(
    tree: &TreeInput,
    state_root: [u8; 32],
    origin_domain: u32,
) -> Result<MerkleTree, AttestError> {
    match tree {
        TreeInput::Evm(proof) => {
            let base_slot = crate::hyperlane_state::merkle_tree_base_slot(origin_domain)
                .ok_or(AttestError::NoTreeLayout { domain: origin_domain })?;
            Ok(get_evm_merkle_tree(state_root.into(), base_slot, proof)?)
        }
        TreeInput::Celestia { hook_id, hook_bytes, proof } => {
            Ok(get_celestia_merkle_tree(state_root, *hook_id, hook_bytes, proof)?)
        }
    }
}

/// The address a tree proof actually reads, as a Hyperlane 32-byte address.
///
/// EVM addresses are 20 bytes and Hyperlane left-pads them; Celestia's hook ids are already
/// 32 bytes.
pub fn tree_address_of(tree: &TreeInput) -> [u8; 32] {
    match tree {
        TreeInput::Evm(proof) => {
            let mut padded = [0u8; 32];
            padded[12..].copy_from_slice(proof.merkle_tree_hook.as_slice());
            padded
        }
        TreeInput::Celestia { hook_id, .. } => *hook_id,
    }
}

/// Which beacon slot it is now, from the enclave's clock and the store's own genesis.
fn current_slot(genesis_time: u64) -> Result<u64, AttestError> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| AttestError::NoClock)?
        .as_secs();
    Ok(secs.saturating_sub(genesis_time) / SECONDS_PER_SLOT)
}

/// Ethereum's slot time, fixed since genesis.
const SECONDS_PER_SLOT: u64 = 12;

/// The enclave's own clock, for the one check that needs wall time.
fn enclave_now() -> Result<tendermint::Time, AttestError> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| AttestError::NoClock)?
        .as_secs();
    tendermint::Time::from_unix_timestamp(secs as i64, 0).map_err(|_| AttestError::NoClock)
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
/// Returns the attested root, the light-client store commitment, and the newest chain time
/// the enclave verified.
///
/// That third value is the origin's own head for a chain with its own light client, and the
/// *L1* head for an L2, whose confirmed head is deliberately old.
fn advance_origin(
    input: &mut OriginInput,
    trusted: &IsmState,
) -> Result<(AttestedRoot, [u8; 32], u64), AttestError> {
    match input {
        OriginInput::Ethereum { store, updates } => {
            if ethereum::commit_ethereum_store(store) != trusted.lc_store_commit {
                return Err(AttestError::StoreCommitmentMismatch);
            }
            // Derived here rather than taken from the request: the store already carries
            // genesis_time, and a caller-named slot is a caller-named clock.
            let slot = current_slot(store.genesis_time)?;
            verify_ethereum_updates(store, updates, slot)?;
            let root = get_ethereum_root(store)?;
            Ok((root, ethereum::commit_ethereum_store(store), root.timestamp))
        }
        OriginInput::Celestia { store, updates } => {
            if celestia::commit_celestia_store(store) != trusted.lc_store_commit {
                return Err(AttestError::StoreCommitmentMismatch);
            }
            verify_celestia_updates(store, updates, enclave_now()?)?;
            let root = get_celestia_root(store)?;
            Ok((root, celestia::commit_celestia_store(store), root.timestamp))
        }
        // An L2's trust chain starts at Ethereum: verify L1 first, then read the L2 root out
        // of L1 storage. The commitment carried in the ISM state is Ethereum's, because
        // Ethereum's light client is the thing with state worth remembering.
        // The L1 head is what dates this attestation. The L2's confirmed head is older by a
        // fraud-proof window, which is a property of the rollup and not evidence of staleness.
        OriginInput::Arbitrum { ethereum, proof } => {
            let (l1, commit, _) = advance_ethereum(ethereum, trusted)?;
            Ok((get_arbitrum_root(l1.state_root, proof)?, commit, l1.timestamp))
        }
        OriginInput::Base { ethereum, proof } => {
            let (l1, commit, _) = advance_ethereum(ethereum, trusted)?;
            Ok((get_base_root(l1.state_root, proof)?, commit, l1.timestamp))
        }
    }
}

fn advance_ethereum(
    input: &mut OriginInput,
    trusted: &IsmState,
) -> Result<(AttestedRoot, [u8; 32], u64), AttestError> {
    match input {
        OriginInput::Ethereum { .. } => advance_origin(input, trusted),
        _ => Err(AttestError::L2NeedsEthereum),
    }
}

/// dstack's quote response, as its unix socket returns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteResponse {
    /// Hex-encoded TDX quote.
    pub quote: String,
    /// The runtime event log, as JSON.
    pub event_log: String,
}
