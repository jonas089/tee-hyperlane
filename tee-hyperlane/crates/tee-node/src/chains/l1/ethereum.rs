//! Ethereum (Sepolia): a sync-committee light client.
//!
//! The enclave checks the sync committee's BLS signatures against a store the ISM already
//! commits to, so the beacon and execution RPCs are data sources only.

use crate::state::IsmState;
use alloy_primitives::{keccak256, Bytes, B256};
use helios_consensus_core::consensus_spec::MainnetConsensusSpec;
use helios_consensus_core::types::LightClientHeader;
use helios_consensus_core::types::{FinalityUpdate, Forks, LightClientStore, Update};
use helios_consensus_core::{
    apply_finality_update, apply_update, verify_finality_update, verify_update,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use ssz::Encode;
use tree_hash::TreeHash;

use crate::origin::{self, AttestedRoot, Chain, Head, Origin, Tree};

pub static ETHEREUM: Chain = Chain {
    name: "ethereum",
    domain: 11155111,
    origin: &Ethereum,
};

/// Hyperlane's canonical Sepolia `MerkleTreeHook` keeps its tree from slot 103.
pub const TREE_SLOT: u64 = 103;
/// Sepolia's slot time, fixed since genesis.
const SECONDS_PER_SLOT: u64 = 12;

/// Sepolia uses mainnet consensus parameters, including a 512-key sync committee.
pub type Spec = MainnetConsensusSpec;

pub struct Ethereum;

/// An Ethereum step: the store the ISM committed to, and the updates that move it forward.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Input {
    pub store: EthereumStore,
    pub updates: Updates,
    /// From Gloas on: the RLP of the execution header the finalized beacon header names by
    /// hash, which is where the state root, number and timestamp now come from.
    #[serde(default)]
    pub execution_header: Option<Bytes>,
}

/// The light client's state, carried by the coprocessor and pinned by `lc_store_commit`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EthereumStore {
    pub store: LightClientStore<Spec>,
    /// Beacon genesis validators root; binds the store to one chain.
    pub genesis_root: B256,
    pub genesis_time: u64,
    pub forks: Forks,
}

/// Sync-committee updates, needed only when a committee period boundary has passed, then the
/// finality update that moves the head.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Updates {
    pub committee_updates: Vec<Update<Spec>>,
    pub finality_update: Option<FinalityUpdate<Spec>>,
}

impl Origin for Ethereum {
    /// Walk the light client forward to a newer finalized head.
    fn verify(&self, input: Value, trusted: &IsmState) -> anyhow::Result<Head> {
        let Input {
            mut store,
            updates,
            execution_header,
        } = origin::parse("ethereum input", input)?;
        anyhow::ensure!(
            store.commitment() == trusted.lc_store_commit,
            "supplied light-client store does not match the commitment in the ISM state"
        );
        anyhow::ensure!(
            !updates.committee_updates.is_empty() || updates.finality_update.is_some(),
            "no updates supplied"
        );
        // The current slot bounds how far ahead an update may claim to be. It comes from the
        // store's genesis and the enclave's clock: a caller-named slot is a caller-named clock.
        let slot = origin::now()?.saturating_sub(store.genesis_time) / SECONDS_PER_SLOT;
        // Progress is measured on the beacon header, which the commitment pins. The execution
        // block of the store's own header is not committed and may not be supplied at all.
        let before = store.store.finalized_header.beacon().slot;

        // Committee updates only when a sync-committee period boundary has passed, then the
        // finality update that moves the head.
        for (i, update) in updates.committee_updates.iter().enumerate() {
            verify_update::<Spec>(update, slot, &store.store, store.genesis_root, &store.forks)
                .map_err(|e| anyhow::anyhow!("sync committee update {i} rejected: {e}"))?;
            apply_update::<Spec>(&mut store.store, update);
        }
        if let Some(finality) = &updates.finality_update {
            verify_finality_update::<Spec>(
                finality,
                slot,
                &store.store,
                store.genesis_root,
                &store.forks,
            )
            .map_err(|e| anyhow::anyhow!("finality update rejected: {e}"))?;
            apply_finality_update::<Spec>(&mut store.store, finality);
        }

        let after = store.store.finalized_header.beacon().slot;
        anyhow::ensure!(
            after > before,
            "finalized head did not advance: still at slot {after}"
        );
        let root = store.root(execution_header.as_ref().map(|b| b.as_ref()))?;
        Ok(Head {
            root: root.state_root,
            height: root.height,
            timestamp: root.timestamp,
            store_commit: store.commitment(),
            attested_at: root.timestamp,
        })
    }

    fn merkle_tree(&self, proof: Value, root: B256) -> anyhow::Result<Tree> {
        crate::evm::read_tree(proof, root, TREE_SLOT)
    }
}

/// What the coprocessor needs too: which head a store is at, and what the ISM commits to.
impl EthereumStore {
    /// The execution state root of the finalized head. Finalized, not optimistic: an
    /// optimistic head can still be reorged, and a bridge minting against it loses funds.
    ///
    /// From Gloas on the header names its execution block only by hash, so `execution_header`
    /// is that block's header RLP, accepted only if it hashes to the proven block hash.
    pub fn root(&self, execution_header: Option<&[u8]>) -> anyhow::Result<AttestedRoot> {
        if let LightClientHeader::Gloas(header) = &self.store.finalized_header {
            let rlp = execution_header
                .ok_or_else(|| anyhow::anyhow!("a Gloas head needs its execution header"))?;
            anyhow::ensure!(
                keccak256(rlp) == header.execution_block_hash,
                "execution header does not hash to the finalized block hash {}",
                header.execution_block_hash
            );
            let block = crate::evm::BlockHeader::decode(rlp)?;
            return Ok(AttestedRoot {
                state_root: block.state_root,
                height: block.number,
                timestamp: block.timestamp,
            });
        }
        let execution = self.store.finalized_header.execution().map_err(|_| {
            anyhow::anyhow!("finalized header is pre-Capella and has no execution payload")
        })?;
        Ok(AttestedRoot {
            state_root: *execution.state_root(),
            height: *execution.block_number(),
            timestamp: *execution.timestamp(),
        })
    }

    /// Commit to everything the next verification depends on. The sync committees matter as
    /// much as the header: without them a relayer could swap in a committee of its choosing.
    pub fn commitment(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"tee-isms/ethereum-store/v2");
        h.update(self.genesis_root.as_slice());
        h.update(self.genesis_time.to_be_bytes());
        h.update(
            self.store
                .finalized_header
                .beacon()
                .tree_hash_root()
                .as_slice(),
        );
        // The forks decide which signing domain each update is checked under. Listed field by
        // field, so a fork added upstream is a compile error here, not an uncommitted field.
        let f = &self.forks;
        for fork in [
            &f.genesis,
            &f.altair,
            &f.bellatrix,
            &f.capella,
            &f.deneb,
            &f.electra,
            &f.fulu,
            &f.gloas,
        ] {
            h.update(fork.epoch.to_be_bytes());
            h.update(fork.fork_version);
        }
        h.update(self.store.current_sync_committee.as_ssz_bytes());
        match &self.store.next_sync_committee {
            Some(next) => {
                h.update([1u8]);
                h.update(next.as_ssz_bytes());
            }
            None => h.update([0u8]),
        }
        h.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_consensus_core::apply_bootstrap;
    use helios_consensus_core::types::{Bootstrap, Fork};

    /// Real Sepolia data across Glamsterdam, shared with the vendored helios tests.
    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/../helios-consensus-core/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap();
        serde_json::from_str(&text).unwrap_or(Value::String(text.trim().to_string()))
    }

    fn store() -> EthereumStore {
        let spec = fixture("spec.json");
        let fork = |name: &str| Fork {
            epoch: spec[format!("{name}_FORK_EPOCH")]
                .as_str()
                .map_or(0, |e| e.parse().unwrap()),
            fork_version: spec[format!("{name}_FORK_VERSION")]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        };
        let genesis = fixture("genesis.json");
        let bootstrap: Bootstrap<Spec> =
            serde_json::from_value(fixture("bootstrap_fulu.json")["data"].clone()).unwrap();
        let mut store = LightClientStore::default();
        apply_bootstrap(&mut store, &bootstrap);
        EthereumStore {
            store,
            genesis_root: genesis["genesis_validators_root"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
            genesis_time: genesis["genesis_time"].as_str().unwrap().parse().unwrap(),
            forks: Forks {
                genesis: Fork {
                    epoch: 0,
                    fork_version: spec["GENESIS_FORK_VERSION"]
                        .as_str()
                        .unwrap()
                        .parse()
                        .unwrap(),
                },
                altair: fork("ALTAIR"),
                bellatrix: fork("BELLATRIX"),
                capella: fork("CAPELLA"),
                deneb: fork("DENEB"),
                electra: fork("ELECTRA"),
                fulu: fork("FULU"),
                gloas: fork("GLOAS"),
            },
        }
    }

    fn input(header: Option<String>) -> Value {
        let committee: Vec<Value> = fixture("updates.json")
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["data"].clone())
            .collect();
        json!({
            "store": store(),
            "updates": { "committee_updates": committee, "finality_update": fixture("finality_update.json")["data"] },
            "execution_header": header,
        })
    }

    fn trusted() -> IsmState {
        IsmState {
            state_root: [0; 32],
            origin_domain: ETHEREUM.domain,
            height: 0,
            timestamp: 0,
            lc_store_commit: store().commitment(),
            identity_digest: [0; 32],
        }
    }

    use serde_json::json;

    #[test]
    fn a_fulu_store_crosses_into_gloas_and_reads_the_execution_header() {
        let header = fixture("execution_header.hex")
            .as_str()
            .unwrap()
            .to_string();
        let block = fixture("execution_block.json");
        let head = Ethereum.verify(input(Some(header)), &trusted()).unwrap();
        let number = u64::from_str_radix(
            block["number"].as_str().unwrap().trim_start_matches("0x"),
            16,
        )
        .unwrap();
        assert_eq!(head.height, number);
        assert_eq!(
            head.root,
            block["stateRoot"]
                .as_str()
                .unwrap()
                .parse::<B256>()
                .unwrap()
        );
        assert_ne!(head.store_commit, trusted().lc_store_commit);
    }

    #[test]
    fn a_gloas_head_needs_the_header_its_hash_names() {
        assert!(Ethereum.verify(input(None), &trusted()).is_err());
        // A real header of another block: decodes fine, hashes to the wrong block.
        let mut other = fixture("execution_header.hex")
            .as_str()
            .unwrap()
            .to_string();
        other.replace_range(other.len() - 2.., "00");
        assert!(Ethereum.verify(input(Some(other)), &trusted()).is_err());
    }

    #[test]
    fn the_commitment_covers_the_gloas_fork() {
        let mut moved = store();
        moved.forks.gloas.epoch += 1;
        assert_ne!(moved.commitment(), store().commitment());
    }
}
