//! The bytes an ISM stores and the enclave attests to.
//!
//! Three decoders read these: this one, `x/teeism` in Go and `TeeDcapIsm.sol`. All fields are
//! big-endian, and one byte of drift between them rejects every attestation.

use sha2::{Digest, Sha256};

/// A trusted state: 116 bytes, the first 32 of them the origin's state root.
pub const ISM_STATE_BYTES: usize = 116;
/// The most message ids one attestation may authorise; both ISMs refuse more.
pub const MAX_MESSAGE_ID_COUNT: u64 = 1_000_000;

const OFF_ORIGIN_DOMAIN: usize = 32;
const OFF_HEIGHT: usize = 36;
const OFF_TIMESTAMP: usize = 44;
const OFF_LC_STORE_COMMIT: usize = 52;
const OFF_IDENTITY_DIGEST: usize = 84;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsmState {
    /// Execution state root (EVM origins) or app hash (Celestia origins).
    pub state_root: [u8; 32],
    /// Hyperlane domain of the origin. Fixed at ISM creation.
    pub origin_domain: u32,
    /// Origin block height.
    pub height: u64,
    /// Origin head timestamp, in seconds. Never decreases.
    pub timestamp: u64,
    /// Commitment to the light-client store the next attestation must start from.
    pub lc_store_commit: [u8; 32],
    /// The enclave identity this ISM pins.
    pub identity_digest: [u8; 32],
}

/// What the enclave hashes into `report_data`: the transition and the batch it authorises.
///
/// `prev_state(116) new_state(116) merkle_tree(32) attested_at(8) count(8) ids(32 * count)`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestedUpdate {
    pub prev_state: IsmState,
    pub new_state: IsmState,
    /// Origin merkle tree hook, left-padded to 32 bytes for EVM addresses.
    pub merkle_tree_address: [u8; 32],
    /// The newest chain time the enclave verified.
    pub attested_at: u64,
    /// The new leaves of the origin tree, in order.
    pub message_ids: Vec<[u8; 32]>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("expected {expected} bytes, got {got}")]
    WrongLength { expected: usize, got: usize },
    #[error("message id count {0} exceeds the maximum")]
    TooManyMessages(u64),
}

const ATTESTED_UPDATE_HEAD: usize = 2 * ISM_STATE_BYTES + 48;

impl IsmState {
    pub fn encode(&self) -> [u8; ISM_STATE_BYTES] {
        let mut out = [0u8; ISM_STATE_BYTES];
        out[..OFF_ORIGIN_DOMAIN].copy_from_slice(&self.state_root);
        out[OFF_ORIGIN_DOMAIN..OFF_HEIGHT].copy_from_slice(&self.origin_domain.to_be_bytes());
        out[OFF_HEIGHT..OFF_TIMESTAMP].copy_from_slice(&self.height.to_be_bytes());
        out[OFF_TIMESTAMP..OFF_LC_STORE_COMMIT].copy_from_slice(&self.timestamp.to_be_bytes());
        out[OFF_LC_STORE_COMMIT..OFF_IDENTITY_DIGEST].copy_from_slice(&self.lc_store_commit);
        out[OFF_IDENTITY_DIGEST..].copy_from_slice(&self.identity_digest);
        out
    }

    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        if b.len() != ISM_STATE_BYTES {
            return Err(DecodeError::WrongLength {
                expected: ISM_STATE_BYTES,
                got: b.len(),
            });
        }
        let bytes32 = |o: usize| -> [u8; 32] { b[o..o + 32].try_into().unwrap() };
        let u64_at = |o: usize| u64::from_be_bytes(b[o..o + 8].try_into().unwrap());
        Ok(IsmState {
            state_root: bytes32(0),
            origin_domain: u32::from_be_bytes(b[OFF_ORIGIN_DOMAIN..OFF_HEIGHT].try_into().unwrap()),
            height: u64_at(OFF_HEIGHT),
            timestamp: u64_at(OFF_TIMESTAMP),
            lc_store_commit: bytes32(OFF_LC_STORE_COMMIT),
            identity_digest: bytes32(OFF_IDENTITY_DIGEST),
        })
    }
}

impl AttestedUpdate {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ATTESTED_UPDATE_HEAD + 32 * self.message_ids.len());
        out.extend_from_slice(&self.prev_state.encode());
        out.extend_from_slice(&self.new_state.encode());
        out.extend_from_slice(&self.merkle_tree_address);
        out.extend_from_slice(&self.attested_at.to_be_bytes());
        out.extend_from_slice(&(self.message_ids.len() as u64).to_be_bytes());
        for id in &self.message_ids {
            out.extend_from_slice(id);
        }
        out
    }

    /// Exactly `count` ids and nothing after them, so one encoding maps to one update.
    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        if b.len() < ATTESTED_UPDATE_HEAD {
            return Err(DecodeError::WrongLength {
                expected: ATTESTED_UPDATE_HEAD,
                got: b.len(),
            });
        }
        let mut o = 2 * ISM_STATE_BYTES;
        let merkle_tree_address: [u8; 32] = b[o..o + 32].try_into().unwrap();
        o += 32;
        let attested_at = u64::from_be_bytes(b[o..o + 8].try_into().unwrap());
        o += 8;
        let count = u64::from_be_bytes(b[o..o + 8].try_into().unwrap());
        o += 8;
        if count > MAX_MESSAGE_ID_COUNT {
            return Err(DecodeError::TooManyMessages(count));
        }
        let expected = o + 32 * count as usize;
        if b.len() != expected {
            return Err(DecodeError::WrongLength {
                expected,
                got: b.len(),
            });
        }
        Ok(AttestedUpdate {
            prev_state: IsmState::decode(&b[..ISM_STATE_BYTES])?,
            new_state: IsmState::decode(&b[ISM_STATE_BYTES..2 * ISM_STATE_BYTES])?,
            merkle_tree_address,
            attested_at,
            message_ids: b[o..]
                .chunks_exact(32)
                .map(|c| c.try_into().unwrap())
                .collect(),
        })
    }

    /// The value in the first 32 bytes of the quote's `report_data`.
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.encode()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(n: u8) -> IsmState {
        IsmState {
            state_root: [n; 32],
            origin_domain: 0x0102_0304,
            height: 0x1122_3344_5566_7788,
            timestamp: 0x99aa_bbcc_ddee_ff00,
            lc_store_commit: [n + 1; 32],
            identity_digest: [n + 2; 32],
        }
    }

    /// The offsets `TeeDcapIsm.sol` and `x/teeism` read, byte for byte.
    #[test]
    fn state_layout_matches_the_isms() {
        let b = state(7).encode();
        assert_eq!(&b[0..32], &[7; 32]);
        assert_eq!(&b[32..36], &[1, 2, 3, 4]);
        assert_eq!(&b[36..44], &0x1122_3344_5566_7788u64.to_be_bytes());
        assert_eq!(&b[44..52], &0x99aa_bbcc_ddee_ff00u64.to_be_bytes());
        assert_eq!(&b[52..84], &[8; 32]);
        assert_eq!(&b[84..116], &[9; 32]);
        assert_eq!(IsmState::decode(&b).unwrap(), state(7));
    }

    #[test]
    fn an_update_round_trips_and_rejects_anything_else() {
        let u = AttestedUpdate {
            prev_state: state(1),
            new_state: state(4),
            merkle_tree_address: [0xab; 32],
            attested_at: 42,
            message_ids: vec![[1; 32], [2; 32]],
        };
        let b = u.encode();
        assert_eq!(b.len(), ATTESTED_UPDATE_HEAD + 64);
        assert_eq!(AttestedUpdate::decode(&b).unwrap(), u);
        assert!(
            AttestedUpdate::decode(&b[..b.len() - 1]).is_err(),
            "truncated"
        );
        assert!(
            AttestedUpdate::decode(&[b.clone(), vec![0]].concat()).is_err(),
            "trailing"
        );
        assert!(IsmState::decode(&[0u8; 115]).is_err());
    }
}
