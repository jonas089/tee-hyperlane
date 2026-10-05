//! Base Sepolia: a block its sequencer signed on the p2p network, and the tree under its root.
//!
//! The OP Stack sequencer signs every block it gossips with the key `SystemConfig` names as
//! `unsafeBlockSigner`. The signed bytes are the execution payload itself, which carries the
//! state root, so the signature is the only check. Pinned here, so the key is part of this
//! image's measurement; a key rotation on L1 needs a new image.

use crate::state::IsmState;
use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use serde_json::Value;

use crate::origin::{self, Chain, Head, Origin, Tree};

pub static BASE: Chain = Chain {
    name: "base",
    domain: CHAIN_ID as u32,
    origin: &Base,
};

/// Base Sepolia. Part of the signed hash, so a block signed for another chain does not verify.
pub const CHAIN_ID: u64 = 84532;
/// `SystemConfig(0xf272…6194).unsafeBlockSigner()` on Sepolia.
pub const SEQUENCER: Address = address!("b830b99c95Ea32300039624Cb567d324D4b1D83C");
/// Base's `MerkleTreeHook` keeps its tree from slot 151.
pub const TREE_SLOT: u64 = 151;

/// The gossip envelope since Ecotone: the parent beacon block root, then the SSZ payload.
const BEACON_ROOT_BYTES: usize = 32;
// Offsets into the SSZ `ExecutionPayload`. All in its fixed-size part, which every payload
// version since Bellatrix shares, so they do not move between forks.
const STATE_ROOT: usize = 52;
const BLOCK_NUMBER: usize = 404;
const TIMESTAMP: usize = 428;
const EXTRA_DATA_OFFSET: usize = 436;
const BLOCK_HASH: usize = 472;
/// The fixed-size part of a v3 payload, and of a v4 one, which adds the withdrawals root.
const FIXED_PART: usize = 528;
const FIXED_PART_V4: usize = 560;

pub struct Base;

/// A block as the sequencer gossiped it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Input {
    /// 65 bytes, `r || s || v`.
    pub signature: Bytes,
    /// Everything after the signature in the decompressed gossip message.
    pub envelope: Bytes,
}

/// What a signed block says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedBlock {
    pub state_root: B256,
    pub number: u64,
    pub timestamp: u64,
    pub hash: B256,
}

impl Origin for Base {
    fn verify(&self, input: Value, trusted: &IsmState) -> anyhow::Result<Head> {
        let input: Input = origin::parse("base input", input)?;
        let block = verify_block(&input)?;
        anyhow::ensure!(
            block.number > trusted.height,
            "signed block {} is not past the trusted height {}",
            block.number,
            trusted.height
        );
        Ok(Head {
            root: block.state_root,
            height: block.number,
            timestamp: block.timestamp,
            // No light client, so nothing to commit to; carried so the state stays as it was.
            store_commit: trusted.lc_store_commit,
            attested_at: block.timestamp,
        })
    }

    fn merkle_tree(&self, proof: Value, root: B256) -> anyhow::Result<Tree> {
        crate::evm::read_tree(proof, root, TREE_SLOT)
    }
}

/// Check the sequencer signed `input`, and read the block out of it. The coprocessor uses this
/// too, to keep only blocks the enclave will accept.
pub fn verify_block(input: &Input) -> anyhow::Result<SignedBlock> {
    super::sequencer::verify(signing_hash(&input.envelope), &input.signature, SEQUENCER)?;
    read_envelope(&input.envelope)
}

/// The block fields of a v3 or v4 gossip envelope.
fn read_envelope(envelope: &[u8]) -> anyhow::Result<SignedBlock> {
    let payload = envelope
        .get(BEACON_ROOT_BYTES..)
        .filter(|p| p.len() >= FIXED_PART)
        .ok_or_else(|| anyhow::anyhow!("gossip envelope is too short for a payload"))?;
    let word = |o: usize| B256::from_slice(&payload[o..o + 32]);
    let uint = |o: usize| u64::from_le_bytes(payload[o..o + 8].try_into().unwrap());
    // Older payloads have no beacon root in front and are signed under the same domain, so
    // one read at the wrong offset would still verify. The first variable-size field starts
    // right after the fixed part, which pins the layout to v3 or v4.
    let first_offset = u32::from_le_bytes(
        payload[EXTRA_DATA_OFFSET..EXTRA_DATA_OFFSET + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    anyhow::ensure!(
        first_offset == FIXED_PART || first_offset == FIXED_PART_V4,
        "not a v3 or v4 execution payload"
    );
    Ok(SignedBlock {
        state_root: word(STATE_ROOT),
        number: uint(BLOCK_NUMBER),
        timestamp: uint(TIMESTAMP),
        hash: word(BLOCK_HASH),
    })
}

/// `keccak(domain || chain id || keccak(envelope))`, with domain zero for blocks, as op-node
/// signs.
fn signing_hash(envelope: &[u8]) -> B256 {
    let mut preimage = [0u8; 96];
    preimage[32..64].copy_from_slice(&U256::from(CHAIN_ID).to_be_bytes::<32>());
    preimage[64..].copy_from_slice(keccak256(envelope).as_slice());
    keccak256(preimage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Fixture {
        message: String,
        number: u64,
        timestamp: u64,
        state_root: B256,
        hash: B256,
    }

    /// A message captured off Base Sepolia's gossip, already snappy-decoded, with the block
    /// as the RPC reports it.
    fn fixture() -> (Input, Fixture) {
        // Read at run time, so the crate still builds before one has been captured.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/base_gossip.json");
        let text = std::fs::read_to_string(path).unwrap_or_else(|_| {
            panic!(
                "no {path}; capture one with tee-coprocessor's `base_gossip_live` test on a host \
                 with a public IP"
            )
        });
        let f: Fixture = serde_json::from_str(&text).unwrap();
        let raw = hex::decode(f.message.trim_start_matches("0x")).unwrap();
        let input = Input {
            signature: Bytes::copy_from_slice(&raw[..65]),
            envelope: Bytes::copy_from_slice(&raw[65..]),
        };
        (input, f)
    }

    #[test]
    fn a_gossiped_block_verifies_and_reads_as_the_rpc_reports_it() {
        let (input, f) = fixture();
        let block = verify_block(&input).unwrap();
        assert_eq!(
            block,
            SignedBlock {
                state_root: f.state_root,
                number: f.number,
                timestamp: f.timestamp,
                hash: f.hash,
            }
        );
    }

    #[test]
    fn any_change_to_the_signed_bytes_is_refused() {
        let (input, _) = fixture();
        let mut envelope = input.envelope.to_vec();
        envelope[BEACON_ROOT_BYTES + STATE_ROOT] ^= 1;
        let changed = Input {
            envelope: envelope.into(),
            ..input.clone()
        };
        assert!(verify_block(&changed).is_err());
        let mut signature = input.signature.to_vec();
        signature[10] ^= 1;
        assert!(verify_block(&Input {
            signature: signature.into(),
            ..input
        })
        .is_err());
    }

    /// A pre-Ecotone message is a bare payload, signed under the same domain. Read as an
    /// envelope it would be 32 bytes off; the layout check refuses it.
    #[test]
    fn a_payload_without_a_beacon_root_is_refused() {
        let (input, _) = fixture();
        assert!(read_envelope(&input.envelope).is_ok());
        let bare = &input.envelope[BEACON_ROOT_BYTES..];
        assert!(read_envelope(bare).is_err());
    }

    #[test]
    fn an_old_block_does_not_move_the_ism() {
        let (input, f) = fixture();
        let trusted = IsmState {
            state_root: [0; 32],
            origin_domain: BASE.domain,
            height: f.number,
            timestamp: 0,
            lc_store_commit: [7; 32],
            identity_digest: [0; 32],
        };
        let value = serde_json::to_value(&input).unwrap();
        assert!(Base.verify(value.clone(), &trusted).is_err());
        let head = Base
            .verify(
                value,
                &IsmState {
                    height: f.number - 1,
                    ..trusted
                },
            )
            .unwrap();
        assert_eq!(head.store_commit, [7; 32], "carried unchanged");
    }
}
