//! Base Sepolia: its state root, read out of Ethereum.
//!
//! Base (OP Stack) settles through dispute games. L1's `AnchorStateRegistry` points at the
//! newest resolved game, the game's code carries the output root it claims, and the output
//! root's preimage names Base's state root. So once Ethereum is verified, Base's root is four
//! checked links away.
//!
//! The registry adopts a game only once its five-day dispute window has closed, so a transfer
//! from Base cannot land sooner than that. Slow by design, not stuck.

use crate::state::IsmState;
use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use serde_json::Value;

use super::l2_shared::{l2_head, Header, Input};
use super::Ethereum;
use crate::evm::{self, ClaimedAccount};
use crate::origin::{self, Chain, Head, Origin, Tree};

pub static BASE: Chain = Chain {
    name: "base",
    domain: 84532,
    origin: &Base,
};

// Where Base's state lives on L1, pinned rather than accepted.
pub const ANCHOR_STATE_REGISTRY: Address = address!("2fF5cC82dBf333Ea30D8ee462178ab1707315355");
/// `anchorGame` in the registry's storage.
pub const ANCHOR_GAME_SLOT: u64 = 2;
/// Where `rootClaim` sits in a dispute game clone's runtime code.
pub const ROOT_CLAIM_OFFSET: usize = 118;
/// Base's `MerkleTreeHook` keeps its tree from slot 151.
pub const TREE_SLOT: u64 = 151;

pub struct Base;

/// Everything that reads Base's root out of L1 state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RootProof {
    pub registry_account: ClaimedAccount,
    pub registry_account_proof: Vec<Bytes>,
    pub anchor_game_proof: Vec<Bytes>,
    /// The whole slot; the game's address is its low 20 bytes.
    pub anchor_game_slot_value: U256,
    /// The game, proven as an account so its code hash is known.
    pub game_account: ClaimedAccount,
    pub game_account_proof: Vec<Bytes>,
    /// The game's runtime code. `rootClaim` is not in storage: these games are clones with
    /// immutable args, so the claim is in the code, and proving the code hash is the only way
    /// to read it under a state root.
    pub game_code: Bytes,
    pub output_root: OutputRoot,
    /// RLP of the L2 header, whose keccak is `output_root.latest_block_hash`. The output root
    /// commits to the block hash, not its height or time, so those come from this header.
    pub l2_header_rlp: Bytes,
}

/// The preimage of an OP Stack output root.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OutputRoot {
    /// Zero for output root version 1.
    pub version: B256,
    pub state_root: B256,
    pub message_passer_storage_root: B256,
    pub latest_block_hash: B256,
}

impl Origin for Base {
    /// Verify Ethereum, then follow four links from its state root to Base's: L1 names the
    /// game, the game's account fixes its code, the code names the claimed output root, and
    /// the output root's preimage names the state root.
    fn verify(&self, input: Value, trusted: &IsmState) -> anyhow::Result<Head> {
        let Input { ethereum, proof } = origin::parse::<Input<RootProof>>("base input", input)?;
        let l1 = Ethereum.verify(ethereum, trusted)?;

        let registry = evm::verify_account_proof(
            l1.root,
            ANCHOR_STATE_REGISTRY,
            &proof.registry_account,
            &proof.registry_account_proof,
        )?;
        evm::verify_storage_proof(
            registry,
            B256::from(U256::from(ANCHOR_GAME_SLOT)),
            proof.anchor_game_slot_value,
            &proof.anchor_game_proof,
        )?;
        let game = Address::from_slice(&proof.anchor_game_slot_value.to_be_bytes::<32>()[12..]);

        // Without the account proof any bytes could claim any root.
        evm::verify_account_proof(
            l1.root,
            game,
            &proof.game_account,
            &proof.game_account_proof,
        )?;
        anyhow::ensure!(
            keccak256(&proof.game_code) == proof.game_account.code_hash,
            "the supplied game code is not the code at {game}"
        );
        let claimed = proof
            .game_code
            .get(ROOT_CLAIM_OFFSET..ROOT_CLAIM_OFFSET + 32)
            .ok_or_else(|| anyhow::anyhow!("game code is too short to hold rootClaim"))?;
        anyhow::ensure!(
            output_root_hash(&proof.output_root).as_slice() == claimed,
            "the output root preimage is not what the game claims"
        );

        anyhow::ensure!(
            keccak256(&proof.l2_header_rlp) == proof.output_root.latest_block_hash,
            "the L2 header is not the block the output root commits to"
        );
        let header = Header::decode(&proof.l2_header_rlp)?;
        // The state root comes from the output root the game claims, which is what the header
        // hash is bound to; height and time come from that header.
        Ok(l2_head(
            l1,
            Header {
                state_root: proof.output_root.state_root,
                ..header
            },
        ))
    }

    fn merkle_tree(&self, proof: Value, root: B256) -> anyhow::Result<Tree> {
        evm::read_tree(proof, root, TREE_SLOT)
    }
}

/// The OP Stack output root: `keccak(version || stateRoot || messagePasserRoot || blockHash)`.
fn output_root_hash(o: &OutputRoot) -> B256 {
    keccak256(
        [
            o.version.as_slice(),
            o.state_root.as_slice(),
            o.message_passer_storage_root.as_slice(),
            o.latest_block_hash.as_slice(),
        ]
        .concat(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Fixture {
        l2_block_number: u64,
        l2_timestamp: u64,
        version: String,
        state_root: String,
        message_passer_storage_root: String,
        latest_block_hash: String,
        header_rlp: String,
    }

    fn fixture() -> (Fixture, OutputRoot) {
        let f: Fixture =
            serde_json::from_str(include_str!("../../testdata/base_output_root.json")).unwrap();
        let o = OutputRoot {
            version: f.version.parse().unwrap(),
            state_root: f.state_root.parse().unwrap(),
            message_passer_storage_root: f.message_passer_storage_root.parse().unwrap(),
            latest_block_hash: f.latest_block_hash.parse().unwrap(),
        };
        (f, o)
    }

    /// Every component is bound, or a relayer could swap in another chain's state root.
    #[test]
    fn the_output_root_binds_every_component() {
        let (_, o) = fixture();
        let base = output_root_hash(&o);
        for i in 0..4 {
            let mut changed = o.clone();
            let field = [
                &mut changed.version,
                &mut changed.state_root,
                &mut changed.message_passer_storage_root,
                &mut changed.latest_block_hash,
            ];
            *field.into_iter().nth(i).unwrap() = B256::repeat_byte(0x33);
            assert_ne!(output_root_hash(&changed), base, "component {i}");
        }
    }

    /// Height and time come from the header the output root's block hash binds, not from the
    /// preimage, which does not commit to them.
    #[test]
    fn the_l2_header_supplies_height_and_time() {
        let (f, o) = fixture();
        let rlp = hex::decode(f.header_rlp.trim_start_matches("0x")).unwrap();
        assert_eq!(keccak256(&rlp), o.latest_block_hash);
        let header = Header::decode(&rlp).unwrap();
        assert_eq!(
            (header.number, header.timestamp),
            (f.l2_block_number, f.l2_timestamp)
        );
        assert_eq!(header.state_root, o.state_root);
    }
}
