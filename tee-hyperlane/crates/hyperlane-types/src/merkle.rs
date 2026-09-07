//! Hyperlane's incremental merkle tree, as implemented by both `MerkleTreeHook.sol` and
//! hyperlane-cosmos.
//!
//! The bridge never inserts into a tree it invented: it adopts the `(branch, count)` proven
//! out of origin-chain state and replays new message ids on top. Because the branch is a
//! function of every leaf ever inserted, a snapshot that is wrong or has gaps cannot
//! reproduce the on-chain branch - which is why the snapshot needs no separate
//! authentication.

use crate::keccak256;

pub const TREE_DEPTH: usize = 32;
pub const MAX_LEAVES: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MerkleTree {
    #[serde(with = "hex_branch")]
    pub branch: [[u8; 32]; TREE_DEPTH],
    pub count: u32,
}

/// Hex strings on the wire: a tree snapshot is read by humans while debugging a stuck
/// route far more often than it is read by machines.
mod hex_branch {
    use super::TREE_DEPTH;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        v: &[[u8; 32]; TREE_DEPTH],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.collect_seq(v.iter().map(hex::encode))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<[[u8; 32]; TREE_DEPTH], D::Error> {
        let items = Vec::<String>::deserialize(d)?;
        if items.len() != TREE_DEPTH {
            return Err(serde::de::Error::custom(format!(
                "branch must have {TREE_DEPTH} entries, got {}",
                items.len()
            )));
        }
        let mut out = [[0u8; 32]; TREE_DEPTH];
        for (slot, text) in out.iter_mut().zip(items) {
            let raw = hex::decode(text.strip_prefix("0x").unwrap_or(&text))
                .map_err(serde::de::Error::custom)?;
            *slot = raw
                .try_into()
                .map_err(|_| serde::de::Error::custom("branch entry must be 32 bytes"))?;
        }
        Ok(out)
    }
}

impl Default for MerkleTree {
    fn default() -> Self {
        Self { branch: [[0u8; 32]; TREE_DEPTH], count: 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MerkleError {
    #[error("merkle tree is full")]
    TreeFull,
}

/// `Z[0] = 0`, `Z[i] = keccak(Z[i-1] || Z[i-1])`.
pub fn zero_hashes() -> [[u8; 32]; TREE_DEPTH] {
    let mut z = [[0u8; 32]; TREE_DEPTH];
    for i in 1..TREE_DEPTH {
        z[i] = keccak_pair(&z[i - 1], &z[i - 1]);
    }
    z
}

pub fn keccak_pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(a);
    buf[32..].copy_from_slice(b);
    keccak256(&buf)
}

pub fn insert_leaf(tree: &mut MerkleTree, leaf: [u8; 32]) -> Result<(), MerkleError> {
    if tree.count >= MAX_LEAVES {
        return Err(MerkleError::TreeFull);
    }
    tree.count += 1;
    let mut node = leaf;
    let mut size = tree.count;
    for level in 0..TREE_DEPTH {
        if size & 1 == 1 {
            tree.branch[level] = node;
            return Ok(());
        }
        node = keccak_pair(&tree.branch[level], &node);
        size /= 2;
    }
    unreachable!("count is bounded by 2^TREE_DEPTH")
}

pub fn get_tree_root(tree: &MerkleTree) -> [u8; 32] {
    let zeros = zero_hashes();
    let mut current = [0u8; 32];
    let mut index = tree.count;
    for level in 0..TREE_DEPTH {
        current = if index & 1 == 1 {
            keccak_pair(&tree.branch[level], &current)
        } else {
            keccak_pair(&current, &zeros[level])
        };
        index /= 2;
    }
    current
}

/// Fold a leaf and its 32-node proof up to a root, exactly as `MerkleLib.branchRoot` does.
pub fn get_branch_root(leaf: [u8; 32], proof: &[[u8; 32]; TREE_DEPTH], index: u32) -> [u8; 32] {
    let mut current = leaf;
    for (level, sibling) in proof.iter().enumerate() {
        let bit = (index >> level) & 1;
        current = if bit == 1 {
            keccak_pair(sibling, &current)
        } else {
            keccak_pair(&current, sibling)
        };
    }
    current
}

// ============================================================================
// Where MerkleTreeHook.sol keeps this tree in storage
// ============================================================================


/// Hyperlane's canonical Sepolia `merkleTreeHook`, confirmed on chain.
pub const SEPOLIA_MERKLE_TREE_BASE_SLOT: u64 = 103;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MerkleTreeSlots {
    pub base: u64,
}

impl MerkleTreeSlots {
    pub const fn new(base: u64) -> Self {
        Self { base }
    }

    /// The 33 slots to request, branch first then count, in the order
    /// [`build_tree_from_slots`] expects.
    pub fn storage_keys(&self) -> [[u8; 32]; TREE_DEPTH + 1] {
        let mut keys = [[0u8; 32]; TREE_DEPTH + 1];
        for (i, key) in keys.iter_mut().enumerate() {
            key[24..].copy_from_slice(&(self.base + i as u64).to_be_bytes());
        }
        keys
    }

    pub fn count_slot(&self) -> u64 {
        self.base + TREE_DEPTH as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("count {0} does not fit in uint32; storage layout is probably wrong")]
    CountNotUint32(u128),
}

/// Assemble a tree from the 33 slot values, in the order [`MerkleTreeSlots::storage_keys`]
/// returns them.
pub fn build_tree_from_slots(
    values: &[[u8; 32]; TREE_DEPTH + 1],
) -> Result<MerkleTree, LayoutError> {
    let mut branch = [[0u8; 32]; TREE_DEPTH];
    branch.copy_from_slice(&values[..TREE_DEPTH]);

    let raw = &values[TREE_DEPTH];
    // Solidity stores count as uint256 but MerkleLib caps it at 2^32; anything above that
    // means we are reading the wrong slot.
    if raw[..28] != [0u8; 28] {
        return Err(LayoutError::CountNotUint32(u128::from_be_bytes(
            raw[16..].try_into().unwrap(),
        )));
    }
    let count = u32::from_be_bytes(raw[28..].try_into().unwrap());
    Ok(MerkleTree { branch, count })
}
