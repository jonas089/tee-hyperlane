//! What the destination chain stores and reads.
//!
//! Two byte formats live here and neither is ours to choose: Celestia's `x/zkism` decodes
//! them in Go, so a single byte of drift rejects every proof the bridge submits. They are
//! pinned by golden tests against celestia-app's own fixtures.

use sha2::{Digest, Sha256};

use crate::CodecError;

// ---------------------------------------------------------------------------
// ISM state
// ---------------------------------------------------------------------------

/// `x/zkism` treats `state` as opaque, interpreting only `state[..32]` as the state root and
/// requiring 32..=2048 bytes overall. Everything past the root is ours to define, so this is
/// where the light-client commitment and the enclave identity digest live.
///
/// All fields are big-endian. The little-endian encodings further down are the exception,
/// forced by the Go decoder.
pub const ISM_STATE_BYTES: usize = 116;

/// The size window `x/zkism` enforces on a state blob.
pub const MIN_STATE_BYTES: usize = 32;
pub const MAX_STATE_BYTES: usize = 2048;
/// The cap `x/zkism` enforces on a message batch.
pub const MAX_MESSAGE_ID_COUNT: u64 = 1_000_000;

const OFF_ORIGIN_DOMAIN: usize = 32;
const OFF_HEIGHT: usize = 36;
const OFF_TIMESTAMP: usize = 44;
const OFF_LC_STORE_COMMIT: usize = 52;
const OFF_IDENTITY_DIGEST: usize = 84;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsmState {
    /// Execution state root (EVM origins) or app hash (Celestia origin).
    pub state_root: [u8; 32],
    /// Hyperlane domain of the origin chain. Fixed at ISM creation, never changes.
    pub origin_domain: u32,
    /// Origin block height, or execution block number.
    pub height: u64,
    /// Origin head timestamp in seconds. Non-decreasing across the state chain.
    pub timestamp: u64,
    /// sha256 over the canonical light-client store the enclave was handed.
    pub lc_store_commit: [u8; 32],
    /// Which enclave may advance this ISM. See [`crate::enclave_identity`].
    pub identity_digest: [u8; 32],
}

pub fn encode_ism_state(s: &IsmState) -> [u8; ISM_STATE_BYTES] {
    let mut out = [0u8; ISM_STATE_BYTES];
    out[..OFF_ORIGIN_DOMAIN].copy_from_slice(&s.state_root);
    out[OFF_ORIGIN_DOMAIN..OFF_HEIGHT].copy_from_slice(&s.origin_domain.to_be_bytes());
    out[OFF_HEIGHT..OFF_TIMESTAMP].copy_from_slice(&s.height.to_be_bytes());
    out[OFF_TIMESTAMP..OFF_LC_STORE_COMMIT].copy_from_slice(&s.timestamp.to_be_bytes());
    out[OFF_LC_STORE_COMMIT..OFF_IDENTITY_DIGEST].copy_from_slice(&s.lc_store_commit);
    out[OFF_IDENTITY_DIGEST..].copy_from_slice(&s.identity_digest);
    out
}

pub fn decode_ism_state(b: &[u8]) -> Result<IsmState, CodecError> {
    if b.len() != ISM_STATE_BYTES {
        return Err(CodecError::WrongLength {
            expected: ISM_STATE_BYTES,
            got: b.len(),
        });
    }
    let at32 = |o: usize| -> [u8; 32] { b[o..o + 32].try_into().unwrap() };
    let be = |o: usize, n: usize| -> u64 {
        let mut buf = [0u8; 8];
        buf[8 - n..].copy_from_slice(&b[o..o + n]);
        u64::from_be_bytes(buf)
    };
    Ok(IsmState {
        state_root: at32(0),
        origin_domain: be(OFF_ORIGIN_DOMAIN, 4) as u32,
        height: be(OFF_HEIGHT, 8),
        timestamp: be(OFF_TIMESTAMP, 8),
        lc_store_commit: at32(OFF_LC_STORE_COMMIT),
        identity_digest: at32(OFF_IDENTITY_DIGEST),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    #[error("origin domain changed; an ISM is pinned to one origin for life")]
    OriginDomainChanged,
    #[error("height did not advance")]
    HeightNotAdvanced,
    #[error("timestamp moved backwards")]
    TimestampWentBackwards,
    #[error("state root unchanged, which would wedge zkism message submission")]
    StateRootUnchanged,
    #[error("enclave identity changed")]
    IdentityChanged,
}

/// The rules every attested transition must satisfy.
///
/// `StateRootUnchanged` is load-bearing rather than cosmetic: `x/zkism` re-arms its
/// one-message-batch-per-root flag only when `state[..32]` differs
/// (`keeper/msg_server.go:97-100`), so a transition that leaves the root alone would stop
/// message submission permanently.
pub fn verify_transition(prev: &IsmState, next: &IsmState) -> Result<(), TransitionError> {
    if prev.origin_domain != next.origin_domain {
        return Err(TransitionError::OriginDomainChanged);
    }
    if next.height <= prev.height {
        return Err(TransitionError::HeightNotAdvanced);
    }
    if next.timestamp < prev.timestamp {
        return Err(TransitionError::TimestampWentBackwards);
    }
    if next.state_root == prev.state_root {
        return Err(TransitionError::StateRootUnchanged);
    }
    if next.identity_digest != prev.identity_digest {
        return Err(TransitionError::IdentityChanged);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// What the enclave attests to
// ---------------------------------------------------------------------------

/// The single payload hashed into a quote's `report_data`. One attestation covers both the
/// state transition and the message batch, so both SP1 proofs rest on the same quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestedUpdate {
    pub prev_state: IsmState,
    pub new_state: IsmState,
    /// Origin merkle tree hook, left-padded to 32 bytes for EVM addresses.
    pub merkle_tree_address: [u8; 32],
    /// The newest chain time the enclave actually verified, which is what the prover's clock
    /// is bounded against.
    ///
    /// Not the same thing as `new_state.timestamp`, and conflating them is what made the two
    /// optimistic-rollup origins impossible. An L2's confirmed head is old *by design* - that
    /// lag is the fraud-proof window - so measuring the prover's clock against it rejects
    /// every honest proof. For an L2 the enclave verified a recent *Ethereum* header on the
    /// way to that root, and that is the time worth bounding against.
    pub attested_at: u64,
    /// Message ids the enclave proved present in the origin tree at `new_state`.
    pub message_ids: Vec<[u8; 32]>,
}

const ATTESTED_UPDATE_HEAD: usize = 2 * ISM_STATE_BYTES + 48;

pub fn encode_attested_update(u: &AttestedUpdate) -> Vec<u8> {
    let mut out = Vec::with_capacity(ATTESTED_UPDATE_HEAD + 32 * u.message_ids.len());
    out.extend_from_slice(&encode_ism_state(&u.prev_state));
    out.extend_from_slice(&encode_ism_state(&u.new_state));
    out.extend_from_slice(&u.merkle_tree_address);
    out.extend_from_slice(&u.attested_at.to_be_bytes());
    out.extend_from_slice(&(u.message_ids.len() as u64).to_be_bytes());
    for id in &u.message_ids {
        out.extend_from_slice(id);
    }
    out
}

pub fn decode_attested_update(b: &[u8]) -> Result<AttestedUpdate, CodecError> {
    if b.len() < ATTESTED_UPDATE_HEAD {
        return Err(CodecError::TooShort {
            needed: ATTESTED_UPDATE_HEAD - b.len(),
        });
    }
    let prev_state = decode_ism_state(&b[..ISM_STATE_BYTES])?;
    let new_state = decode_ism_state(&b[ISM_STATE_BYTES..2 * ISM_STATE_BYTES])?;
    let mut o = 2 * ISM_STATE_BYTES;
    let merkle_tree_address: [u8; 32] = b[o..o + 32].try_into().unwrap();
    o += 32;
    let attested_at = u64::from_be_bytes(b[o..o + 8].try_into().unwrap());
    o += 8;
    let count = u64::from_be_bytes(b[o..o + 8].try_into().unwrap());
    o += 8;
    let message_ids = read_ids(&b[o..], count)?;
    Ok(AttestedUpdate {
        prev_state,
        new_state,
        merkle_tree_address,
        attested_at,
        message_ids,
    })
}

/// The value the enclave puts in the first 32 bytes of `report_data`.
pub fn hash_attested_update(u: &AttestedUpdate) -> [u8; 32] {
    Sha256::digest(encode_attested_update(u)).into()
}

// ---------------------------------------------------------------------------
// x/zkism public values (little-endian; mirrors Rust bincode's defaults)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateTransitionValues {
    pub state: Vec<u8>,
    pub new_state: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateMembershipValues {
    pub state_root: [u8; 32],
    pub merkle_tree_address: [u8; 32],
    pub message_ids: Vec<[u8; 32]>,
}

/// `u64_le(len) || state || u64_le(len) || new_state`
pub fn encode_state_transition_values(state: &[u8], new_state: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + state.len() + new_state.len());
    out.extend_from_slice(&(state.len() as u64).to_le_bytes());
    out.extend_from_slice(state);
    out.extend_from_slice(&(new_state.len() as u64).to_le_bytes());
    out.extend_from_slice(new_state);
    out
}

/// `root(32) || merkle_tree(32) || u64_le(count) || ids`
pub fn encode_state_membership_values(
    state_root: [u8; 32],
    merkle_tree_address: [u8; 32],
    message_ids: &[[u8; 32]],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(72 + 32 * message_ids.len());
    out.extend_from_slice(&state_root);
    out.extend_from_slice(&merkle_tree_address);
    out.extend_from_slice(&(message_ids.len() as u64).to_le_bytes());
    for id in message_ids {
        out.extend_from_slice(id);
    }
    out
}

/// Mirrors the Go decoder, including its tolerance of trailing bytes.
pub fn decode_state_transition_values(data: &[u8]) -> Result<StateTransitionValues, CodecError> {
    let (state, rest) = read_length_prefixed(data)?;
    let (new_state, _) = read_length_prefixed(rest)?;
    Ok(StateTransitionValues { state, new_state })
}

/// Mirrors the Go decoder, including its strict rejection of trailing bytes.
pub fn decode_state_membership_values(data: &[u8]) -> Result<StateMembershipValues, CodecError> {
    if data.len() < 72 {
        return Err(CodecError::TooShort {
            needed: 72 - data.len(),
        });
    }
    let state_root: [u8; 32] = data[..32].try_into().unwrap();
    let merkle_tree_address: [u8; 32] = data[32..64].try_into().unwrap();
    let count = u64::from_le_bytes(data[64..72].try_into().unwrap());
    if count > MAX_MESSAGE_ID_COUNT {
        return Err(CodecError::MessageIdCountTooLarge(count));
    }
    let message_ids = read_ids(&data[72..], count)?;
    Ok(StateMembershipValues {
        state_root,
        merkle_tree_address,
        message_ids,
    })
}

fn read_length_prefixed(data: &[u8]) -> Result<(Vec<u8>, &[u8]), CodecError> {
    if data.len() < 8 {
        return Err(CodecError::TooShort {
            needed: 8 - data.len(),
        });
    }
    let len = u64::from_le_bytes(data[..8].try_into().unwrap());
    if len < MIN_STATE_BYTES as u64 || len > MAX_STATE_BYTES as u64 {
        return Err(CodecError::StateLengthOutOfRange(len));
    }
    let end = 8 + len as usize;
    if data.len() < end {
        return Err(CodecError::TooShort {
            needed: end - data.len(),
        });
    }
    Ok((data[8..end].to_vec(), &data[end..]))
}

/// Read exactly `count` ids, rejecting both truncation and trailing bytes.
fn read_ids(rest: &[u8], count: u64) -> Result<Vec<[u8; 32]>, CodecError> {
    if count > MAX_MESSAGE_ID_COUNT {
        return Err(CodecError::MessageIdCountTooLarge(count));
    }
    let needed = (count as usize).saturating_mul(32);
    if rest.len() < needed {
        return Err(CodecError::TooShort {
            needed: needed - rest.len(),
        });
    }
    if rest.len() > needed {
        return Err(CodecError::TrailingBytes {
            count: rest.len() - needed,
        });
    }
    Ok(rest
        .chunks_exact(32)
        .map(|c| c.try_into().unwrap())
        .collect())
}
