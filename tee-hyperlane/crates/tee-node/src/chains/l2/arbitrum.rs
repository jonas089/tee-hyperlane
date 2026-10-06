//! Arbitrum Sepolia: a block its sequencer signed on the feed, and the tree under its root.
//!
//! The sequencer signs every feed message, and the signature covers the hash of the block the
//! message produced. A header that hashes to it gives the state root. The key is pinned here,
//! so it is part of this image's measurement; a key rotation on L1 needs a new image.

use crate::state::IsmState;
use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use serde_json::Value;

use crate::origin::{self, Chain, Head, Origin, Tree};

pub static ARBITRUM: Chain = Chain {
    name: "arbitrum",
    domain: CHAIN_ID as u32,
    origin: &Arbitrum,
};

/// Arbitrum Sepolia. Part of the signed hash, so a message signed for another chain does not
/// verify.
pub const CHAIN_ID: u64 = 421614;
/// The feed's signer. `SequencerInbox(0x6c97…be0D).isSequencer` is true for it on Sepolia.
pub const SEQUENCER: Address = address!("9396c22161c821231ad4ae8fcf991b4beee39990");
/// Arbitrum's `MerkleTreeHook` keeps its tree from slot 151.
pub const TREE_SLOT: u64 = 151;

pub struct Arbitrum;

/// A feed message and the header of the block it produced.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Input {
    pub message: FeedMessage,
    /// RLP of the block header, whose keccak must be `message.block_hash`.
    pub header_rlp: Bytes,
}

/// One message from the sequencer feed, with the fields its signature covers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FeedMessage {
    pub sequence_number: u64,
    pub block_hash: B256,
    #[serde(default)]
    pub block_metadata: Bytes,
    pub delayed_messages_read: u64,
    pub kind: u8,
    pub sender: Address,
    pub l1_block_number: u64,
    pub timestamp: u64,
    pub request_id: Option<B256>,
    pub l1_base_fee: Option<U256>,
    pub l2_msg: Bytes,
    /// `signatureV2`: 65 bytes, `r || s || v`.
    pub signature: Bytes,
}

/// The three header fields the root needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub state_root: B256,
    pub number: u64,
    pub timestamp: u64,
}

impl Origin for Arbitrum {
    fn verify(&self, input: Value, trusted: &IsmState) -> anyhow::Result<Head> {
        let input: Input = origin::parse("arbitrum input", input)?;
        super::sequencer::verify(
            signing_hash(&input.message),
            &input.message.signature,
            SEQUENCER,
        )?;
        anyhow::ensure!(
            keccak256(&input.header_rlp) == input.message.block_hash,
            "the header is not the block the sequencer signed"
        );
        let header = Header::decode(&input.header_rlp)?;
        anyhow::ensure!(
            header.number > trusted.height,
            "signed block {} is not past the trusted height {}",
            header.number,
            trusted.height
        );
        Ok(Head {
            root: header.state_root,
            height: header.number,
            timestamp: header.timestamp,
            // No light client, so nothing to commit to; carried so the state stays as it was.
            store_commit: trusted.lc_store_commit,
            attested_at: header.timestamp,
        })
    }

    fn merkle_tree(&self, proof: Value, root: B256) -> anyhow::Result<Tree> {
        crate::evm::read_tree(proof, root, TREE_SLOT)
    }
}

/// The hash Nitro's sequencer signs for a feed message: a fixed prefix, the chain id and
/// sequence number, the block hash and metadata, then the message as it was sequenced. The
/// L1 base fee is in its minimal big-endian form.
pub fn signing_hash(m: &FeedMessage) -> B256 {
    let mut d = Vec::with_capacity(160 + m.l2_msg.len());
    d.extend_from_slice(b"Arbitrum Nitro Feed:");
    d.extend_from_slice(&CHAIN_ID.to_be_bytes());
    d.extend_from_slice(&m.sequence_number.to_be_bytes());
    d.extend_from_slice(m.block_hash.as_slice());
    d.extend_from_slice(&m.block_metadata);
    d.extend_from_slice(&m.delayed_messages_read.to_be_bytes());
    d.push(m.kind);
    d.extend_from_slice(m.sender.as_slice());
    d.extend_from_slice(&m.l1_block_number.to_be_bytes());
    d.extend_from_slice(&m.timestamp.to_be_bytes());
    if let Some(id) = m.request_id {
        d.extend_from_slice(id.as_slice());
    }
    if let Some(fee) = m.l1_base_fee {
        d.extend_from_slice(&fee.to_be_bytes_trimmed_vec());
    }
    d.extend_from_slice(&m.l2_msg);
    keccak256(d)
}

impl Header {
    /// A block header is an RLP list: state root is item 3, number item 8, timestamp item 11.
    pub fn decode(rlp: &[u8]) -> anyhow::Result<Self> {
        let malformed = || anyhow::anyhow!("block header RLP is malformed");
        let mut slice = rlp;
        let alloy_rlp::PayloadView::List(items) =
            alloy_rlp::Header::decode_raw(&mut slice).map_err(|_| malformed())?
        else {
            return Err(malformed());
        };
        anyhow::ensure!(slice.is_empty() && items.len() >= 12, malformed());
        let field = |i: usize| {
            let mut item = items[i];
            alloy_rlp::Header::decode_bytes(&mut item, false).map_err(|_| malformed())
        };
        let uint = |i: usize| -> anyhow::Result<u64> {
            let bytes = field(i)?;
            anyhow::ensure!(bytes.len() <= 8, malformed());
            let mut out = [0u8; 8];
            out[8 - bytes.len()..].copy_from_slice(bytes);
            Ok(u64::from_be_bytes(out))
        };
        let root = field(3)?;
        anyhow::ensure!(root.len() == 32, malformed());
        Ok(Self {
            state_root: B256::from_slice(root),
            number: uint(8)?,
            timestamp: uint(11)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Fixture {
        input: Input,
        number: u64,
        timestamp: u64,
        state_root: B256,
    }

    /// A feed message captured off Arbitrum Sepolia, and the header of the block it produced.
    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../../../testdata/arbitrum_feed.json")).unwrap()
    }

    fn trusted(height: u64) -> IsmState {
        IsmState {
            state_root: [0; 32],
            origin_domain: ARBITRUM.domain,
            height,
            timestamp: 0,
            lc_store_commit: [7; 32],
            identity_digest: [0; 32],
        }
    }

    #[test]
    fn every_signed_field_is_bound() {
        let f = fixture();
        let base = signing_hash(&f.input.message);
        let changes: Vec<fn(&mut FeedMessage)> = vec![
            |m| m.sequence_number += 1,
            |m| m.block_hash = B256::repeat_byte(1),
            |m| m.block_metadata = Bytes::from_static(&[9]),
            |m| m.delayed_messages_read += 1,
            |m| m.kind ^= 1,
            |m| m.sender = Address::repeat_byte(1),
            |m| m.l1_block_number += 1,
            |m| m.timestamp += 1,
            |m| m.request_id = Some(B256::repeat_byte(1)),
            |m| m.l1_base_fee = Some(m.l1_base_fee.unwrap_or_default() + U256::from(1)),
            |m| m.l2_msg = Bytes::from_static(&[1]),
        ];
        for (i, change) in changes.iter().enumerate() {
            let mut m = f.input.message.clone();
            change(&mut m);
            assert_ne!(signing_hash(&m), base, "field {i}");
        }
    }

    #[test]
    fn a_header_that_is_not_the_signed_block_is_refused() {
        let mut f = fixture();
        let mut rlp = f.input.header_rlp.to_vec();
        let last = rlp.len() - 1;
        rlp[last] ^= 1;
        f.input.header_rlp = rlp.into();
        assert!(Arbitrum
            .verify(serde_json::to_value(&f.input).unwrap(), &trusted(0))
            .is_err());
    }

    #[test]
    fn a_header_decodes_at_the_expected_positions() {
        let f = fixture();
        let header = Header::decode(&f.input.header_rlp).unwrap();
        assert_eq!(header.state_root, f.state_root);
        assert!(Header::decode(&f.input.header_rlp[..f.input.header_rlp.len() / 2]).is_err());
        assert!(Header::decode(b"").is_err());
    }

    #[test]
    fn a_signed_block_gives_its_header_root() {
        let f = fixture();
        let head = Arbitrum
            .verify(
                serde_json::to_value(&f.input).unwrap(),
                &trusted(f.number - 1),
            )
            .unwrap();
        assert_eq!(
            (head.root, head.height, head.timestamp),
            (f.state_root, f.number, f.timestamp)
        );
        assert_eq!(head.store_commit, [7; 32], "carried unchanged");
        assert!(Arbitrum
            .verify(serde_json::to_value(&f.input).unwrap(), &trusted(f.number))
            .is_err());
    }
}
