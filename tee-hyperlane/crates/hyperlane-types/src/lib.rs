//! Hyperlane wire formats and the incremental merkle tree, written once and shared by the
//! enclave, the coprocessor and the CLI.

pub mod merkle;
pub mod message;

pub use merkle::{
    build_tree_from_slots, get_branch_root, get_tree_root, insert_leaf, keccak_pair, zero_hashes,
    LayoutError, MerkleError, MerkleTree, MerkleTreeSlots, SEPOLIA_MERKLE_TREE_BASE_SLOT,
    TREE_DEPTH,
};
pub use message::{
    decode_hyperlane_message, decode_token_message_body, encode_hyperlane_message,
    encode_token_message_body, get_message_id, HyperlaneMessage, MessageTooShort, TokenMessage,
};

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    use tiny_keccak::{Hasher, Keccak};
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(data);
    k.finalize(&mut out);
    out
}
