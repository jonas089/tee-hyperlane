//! The attested merkle tree address must be the one the tree was actually proven at.
//!
//! Both come from the caller, and proving a tree is not enough on its own: anyone can deploy
//! a merkle tree hook, insert ids of their choosing, and prove it honestly under the real
//! state root. If the attested address were free, that forged tree could be passed off as the
//! canonical hook, and the destination ISM - which only checks the address against its own
//! pinned value - would authorise every id in it.

use alloy_primitives::Address;
use tee_node::attest::{tree_address_of, TreeInput};
use tee_node::hyperlane_state::EvmTreeProof;
use tee_node::state_proofs::ClaimedAccount;

fn evm_proof(hook: &str) -> TreeInput {
    TreeInput::Evm(EvmTreeProof {
        merkle_tree_hook: hook.parse::<Address>().unwrap(),
        base_slot: 103,
        account: ClaimedAccount {
            nonce: 0,
            balance: Default::default(),
            storage_root: Default::default(),
            code_hash: Default::default(),
        },
        account_proof: Vec::new(),
        storage_proof: Vec::new(),
    })
}

/// Hyperlane addresses are 32 bytes; an EVM one is left-padded into them.
#[test]
fn an_evm_hook_maps_to_its_padded_address() {
    let hook = "0x4917a9746A7B6E0A57159cCb7F5a6744247f2d0d";
    let padded = tree_address_of(&evm_proof(hook));

    assert_eq!(&padded[..12], &[0u8; 12], "the high twelve bytes are padding");
    assert_eq!(
        hex::encode(&padded[12..]),
        hook.trim_start_matches("0x").to_lowercase()
    );
}

/// The check that closes the hole: a tree proven at an attacker's hook does not match the
/// canonical address, so it cannot be attested as if it were.
#[test]
fn a_different_hook_does_not_match_the_canonical_one() {
    let canonical = tree_address_of(&evm_proof("0x4917a9746A7B6E0A57159cCb7F5a6744247f2d0d"));
    let attacker = tree_address_of(&evm_proof("0x000000000000000000000000000000000000dEaD"));
    assert_ne!(canonical, attacker);
}

#[test]
fn a_celestia_hook_id_is_used_as_is() {
    let mut id = [0u8; 32];
    id[..20].copy_from_slice(b"router_post_dispatch");
    let tree = TreeInput::Celestia {
        hook_id: id,
        hook_bytes: Vec::new(),
        proof: Default::default(),
    };
    assert_eq!(tree_address_of(&tree), id);
}
