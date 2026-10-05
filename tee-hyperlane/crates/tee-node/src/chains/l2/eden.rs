//! Eden: an EVM chain whose sequencer posts signed headers to Celestia (mocha).
//!
//! Eden's root rests on two checks:
//!
//! 1. Celestia's light client verifies the block the header was posted in, and
//! 2. the pinned sequencer key signed the header, which carries the state root.

use crate::state::IsmState;
use alloy_primitives::{hex, B256};
use celestia_types::namespace_data::{NamespaceData, NamespaceDataId};
use celestia_types::nmt::Namespace;
use celestia_types::{Blob, DataAvailabilityHeader};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::Value;

use crate::chains::l1::celestia::{Celestia, Input as CelestiaInput};
use crate::origin::{self, Chain, Head, Origin, Tree};

pub static EDEN: Chain = Chain {
    name: "eden",
    domain: 3735928814,
    origin: &Eden,
};

// Which chain this is and who may speak for it. Read off the chain: the namespace and key
// were recovered by finding a Celestia blob carrying Eden's own state root and checking that
// all 657 signed headers in it verify under this key.
const NAMESPACE: [u8; 28] = hex!("0000000000000000000000000000000000005d2e074163aa3b4d9818");
const SEQUENCER: [u8; 32] =
    hex!("4366433b4309d4f077f0cc1f4370a525736df9a1dc9a205b8d2db1d630b68d51");
/// The chain id a signed header must name, so one sequencer signing for two chains cannot have
/// a header from one accepted as the other.
const CHAIN_ID: &str = "edennet-2";
/// Eden's `MerkleTreeHook` is the L2s' build; slot 183 holds the count, and `count()` agrees.
pub const TREE_SLOT: u64 = 151;

pub struct Eden;

/// An Eden step: Celestia's step, untouched, then the signed header inside the block it
/// verifies.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Input {
    pub celestia: Value,
    pub proof: HeaderProof,
}

/// Everything needed to find a signed header in a verified Celestia block. All of it is
/// checked; none of it is believed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HeaderProof {
    /// Must hash to the `data_hash` of the header the light client verified.
    pub dah: DataAvailabilityHeader,
    /// Every row of Eden's namespace in that block, with proofs. The whole namespace, because
    /// that proves these are *all* the blobs in it, not just some.
    pub data: NamespaceData,
    /// Which Eden height to attest out of those the block carries. The caller names it
    /// because it can only prove the tree at heights it captured a proof for.
    pub target_height: u64,
}

/// A header the sequencer signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedHeader {
    pub height: u64,
    /// Rollkit carries nanoseconds.
    pub time_ns: u64,
    pub state_root: [u8; 32],
}

impl Origin for Eden {
    fn verify(&self, input: Value, trusted: &IsmState) -> anyhow::Result<Head> {
        let Input { celestia, proof } = origin::parse("eden input", input)?;
        // Celestia's light client first. The block it lands on is the last light block it
        // was given, which it has now verified.
        let celestia_head = Celestia.verify(celestia.clone(), trusted)?;
        let celestia: CelestiaInput = origin::parse("celestia input", celestia)?;
        let block = &celestia
            .updates
            .last()
            .ok_or_else(|| anyhow::anyhow!("no celestia block supplied"))?
            .signed_header
            .header;

        // 1. The rows are really Eden's namespace in the block the light client verified.
        let data_hash = block
            .data_hash
            .ok_or_else(|| anyhow::anyhow!("celestia header has no data hash"))?;
        anyhow::ensure!(
            proof.dah.hash() == data_hash,
            "the DA header is not the verified block's"
        );
        let id = NamespaceDataId::new(Eden::namespace(), block.height.value())?;
        proof
            .data
            .verify(id, &proof.dah)
            .map_err(|e| anyhow::anyhow!("namespace proof rejected: {e}"))?;

        // 2. The sequencer signed the target header.
        let header = Eden::signed_headers(&proof.data)
            .into_iter()
            .find(|h| h.height == proof.target_height)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no header at {} signed by the pinned sequencer",
                    proof.target_height
                )
            })?;

        anyhow::ensure!(
            header.height > trusted.height,
            "signed header {} is not past the trusted height {}",
            header.height,
            trusted.height
        );

        // The Celestia block dates the attestation; the Eden header dates the state.
        Ok(Head {
            root: B256::from(header.state_root),
            height: header.height,
            timestamp: header.time_ns / 1_000_000_000,
            store_commit: celestia_head.store_commit,
            attested_at: celestia_head.timestamp,
        })
    }

    fn merkle_tree(&self, proof: Value, root: B256) -> anyhow::Result<Tree> {
        crate::evm::read_tree(proof, root, TREE_SLOT)
    }
}

impl Eden {
    /// Eden's pinned Celestia namespace. The coprocessor reads Eden's posts out of it too.
    pub fn namespace() -> Namespace {
        Namespace::new_v0(&NAMESPACE[18..]).expect("the pinned namespace is well formed")
    }

    /// Every header in these rows that the pinned sequencer signed. Anyone may write to a Celestia
    /// namespace, so anything else is skipped rather than fatal. The coprocessor uses this too, to
    /// see which heights a block carries.
    pub fn signed_headers(data: &NamespaceData) -> Vec<SignedHeader> {
        let shares: Vec<_> = data
            .rows()
            .iter()
            .flat_map(|r| r.shares.iter().cloned())
            .collect();
        let Ok(blobs) = Blob::reconstruct_all(shares.iter()) else {
            return Vec::new();
        };
        let key = VerifyingKey::from_bytes(&SEQUENCER).expect("the pinned sequencer key is valid");
        blobs
            .iter()
            .filter(|blob| blob.namespace == Eden::namespace())
            .filter_map(|blob| {
                let (payload, signature, signer) = protobuf::signed_data(&blob.data).ok()?;
                // Over the payload exactly as it arrived, never a re-encoding: two encodings of one
                // message are both valid protobuf and only one of them was signed.
                (signer == SEQUENCER
                    && key
                        .verify(payload, &Signature::from_slice(signature).ok()?)
                        .is_ok())
                .then(|| protobuf::header(payload).ok())
                .flatten()
            })
            .collect()
    }
}

/// A small, strict protobuf reader for rollkit's signed headers. Strict because the bytes are
/// attacker-supplied: a repeated field is refused rather than last-one-wins, since that is how
/// one blob gets read two ways.
mod protobuf {
    use super::{SignedHeader, CHAIN_ID};

    enum Wire<'a> {
        Varint(u64),
        Bytes(&'a [u8]),
    }

    /// Every field in `buf`, or an error on anything truncated or ambiguous.
    fn fields(buf: &[u8]) -> anyhow::Result<Vec<(u32, Wire<'_>)>> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        let varint = |pos: &mut usize| -> anyhow::Result<u64> {
            let mut value = 0u64;
            for shift in (0..64).step_by(7) {
                let b = *buf
                    .get(*pos)
                    .ok_or_else(|| anyhow::anyhow!("truncated varint"))?;
                *pos += 1;
                value |= u64::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    return Ok(value);
                }
            }
            anyhow::bail!("varint too long")
        };
        while pos < buf.len() {
            let key = varint(&mut pos)?;
            let field = (key >> 3) as u32;
            let wire = match key & 7 {
                0 => Wire::Varint(varint(&mut pos)?),
                2 => {
                    let len = varint(&mut pos)? as usize;
                    let bytes = buf
                        .get(pos..pos + len)
                        .ok_or_else(|| anyhow::anyhow!("length overruns the buffer"))?;
                    pos += len;
                    Wire::Bytes(bytes)
                }
                1 => {
                    pos += 8;
                    Wire::Varint(0)
                }
                5 => {
                    pos += 4;
                    Wire::Varint(0)
                }
                _ => anyhow::bail!("group wire types are not accepted"),
            };
            anyhow::ensure!(
                !out.iter().any(|(f, _)| *f == field),
                "field {field} repeated"
            );
            out.push((field, wire));
        }
        Ok(out)
    }

    fn bytes<'a>(fields: &[(u32, Wire<'a>)], n: u32) -> anyhow::Result<&'a [u8]> {
        match fields.iter().find(|(f, _)| *f == n) {
            Some((_, Wire::Bytes(b))) => Ok(b),
            _ => anyhow::bail!("missing field {n}"),
        }
    }

    fn uint(fields: &[(u32, Wire<'_>)], n: u32) -> anyhow::Result<u64> {
        match fields.iter().find(|(f, _)| *f == n) {
            Some((_, Wire::Varint(v))) => Ok(*v),
            _ => anyhow::bail!("missing field {n}"),
        }
    }

    /// `SignedData { data = 1, signature = 2, signer = 3 }`, with `Signer { pub_key = 2 }` and
    /// `pub_key` the 4-byte ed25519 tag `08011220` then the key.
    pub fn signed_data(buf: &[u8]) -> anyhow::Result<(&[u8], &[u8], [u8; 32])> {
        let f = fields(buf)?;
        let signature = bytes(&f, 2)?;
        anyhow::ensure!(signature.len() == 64, "signature is not 64 bytes");
        let key = bytes(&fields(bytes(&f, 3)?)?, 2)?;
        anyhow::ensure!(
            key.len() == 36 && key[..4] == [0x08, 0x01, 0x12, 0x20],
            "signer is not a 32-byte ed25519 key"
        );
        Ok((bytes(&f, 1)?, signature, key[4..].try_into()?))
    }

    /// The rollkit header: height 2, time 3, state root 8, chain id 12. Field numbers checked
    /// against Eden's own blocks for twelve heights out of one DA batch.
    pub fn header(buf: &[u8]) -> anyhow::Result<SignedHeader> {
        let f = fields(buf)?;
        // One sequencer may sign for several chains, so a header is only ours if it says so.
        anyhow::ensure!(
            bytes(&f, 12)? == CHAIN_ID.as_bytes(),
            "header is for another chain"
        );
        Ok(SignedHeader {
            height: uint(&f, 2)?,
            time_ns: uint(&f, 3)?,
            state_root: bytes(&f, 8)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("state root is not 32 bytes"))?,
        })
    }
}
