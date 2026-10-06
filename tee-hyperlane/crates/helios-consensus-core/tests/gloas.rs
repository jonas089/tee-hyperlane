//! Gloas against real Sepolia data, from Lodestar v1.49.0 on 2026-10-06: the fork landed at slot
//! 11296768 (epoch 353024), the first slot of sync-committee period 1379.

use alloy::primitives::B256;
use helios_consensus_core::consensus_spec::MainnetConsensusSpec as Spec;
use helios_consensus_core::types::{
    Bootstrap, FinalityUpdate, Fork, Forks, LightClientHeader, LightClientStore, Update,
};
use helios_consensus_core::{
    apply_bootstrap, apply_finality_update, apply_update, verify_bootstrap,
    verify_finality_update, verify_update,
};
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn root(name: &str) -> B256 {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

fn forks() -> Forks {
    let spec = fixture("spec.json");
    let fork = |name: &str| Fork {
        epoch: spec[format!("{name}_FORK_EPOCH")].as_str().map_or(0, |e| e.parse().unwrap()),
        fork_version: spec[format!("{name}_FORK_VERSION")].as_str().unwrap().parse().unwrap(),
    };
    Forks {
        genesis: Fork {
            epoch: 0,
            fork_version: spec["GENESIS_FORK_VERSION"].as_str().unwrap().parse().unwrap(),
        },
        altair: fork("ALTAIR"),
        bellatrix: fork("BELLATRIX"),
        capella: fork("CAPELLA"),
        deneb: fork("DENEB"),
        electra: fork("ELECTRA"),
        fulu: fork("FULU"),
        gloas: fork("GLOAS"),
    }
}

fn genesis_root() -> B256 {
    fixture("genesis.json")["genesis_validators_root"].as_str().unwrap().parse().unwrap()
}

fn store_from(name: &str) -> LightClientStore<Spec> {
    let bootstrap: Bootstrap<Spec> = serde_json::from_value(fixture(name)["data"].clone()).unwrap();
    let mut store = LightClientStore::default();
    apply_bootstrap(&mut store, &bootstrap);
    store
}

const NOW: u64 = 20_000_000;

#[test]
fn the_fork_is_where_sepolia_put_it() {
    let f = forks();
    assert_eq!(f.gloas.epoch, 353_024);
    assert_eq!(f.gloas.epoch * 32, 11_296_768);
}

#[test]
fn a_gloas_bootstrap_verifies_and_carries_the_block_hash() {
    let bootstrap: Bootstrap<Spec> =
        serde_json::from_value(fixture("bootstrap_gloas.json")["data"].clone()).unwrap();
    assert!(matches!(bootstrap, Bootstrap::Gloas(_)));
    assert!(matches!(bootstrap.header(), LightClientHeader::Gloas(_)));
    verify_bootstrap::<Spec>(&bootstrap, root("bootstrap_gloas.root"), &forks()).unwrap();

    // Any other checkpoint, or Fulu's proof shapes for a Gloas slot, is refused.
    assert!(verify_bootstrap::<Spec>(&bootstrap, B256::repeat_byte(1), &forks()).is_err());
    let mut fulu_rules = forks();
    fulu_rules.gloas.epoch = u64::MAX;
    assert!(verify_bootstrap::<Spec>(&bootstrap, root("bootstrap_gloas.root"), &fulu_rules).is_err());
}

#[test]
fn a_tampered_block_hash_is_refused() {
    let mut raw = fixture("bootstrap_gloas.json")["data"].clone();
    raw["header"]["execution_block_hash"] = Value::String(format!("0x{}", "11".repeat(32)));
    let bootstrap: Bootstrap<Spec> = serde_json::from_value(raw).unwrap();
    assert!(verify_bootstrap::<Spec>(&bootstrap, root("bootstrap_gloas.root"), &forks()).is_err());
}

/// What ark's Sepolia ISM has to do: a store from just before the fork, a committee update for
/// its period, the first Gloas update across the boundary, then today's finality update.
#[test]
fn a_fulu_store_walks_across_the_fork() {
    let forks = forks();
    let genesis = genesis_root();
    let mut store = store_from("bootstrap_fulu.json");
    assert!(!matches!(store.finalized_header, LightClientHeader::Gloas(_)));
    let before = store.finalized_header.beacon().slot;

    let updates: Vec<Update<Spec>> = fixture("updates.json")
        .as_array()
        .unwrap()
        .iter()
        .map(|u| serde_json::from_value(u["data"].clone()).unwrap())
        .collect();
    assert!(matches!(updates[0], Update::Electra(_)));
    assert!(matches!(updates[1], Update::Gloas(_)));
    for (i, update) in updates.iter().enumerate() {
        verify_update::<Spec>(update, NOW, &store, genesis, &forks)
            .unwrap_or_else(|e| panic!("update {i}: {e}"));
        apply_update::<Spec>(&mut store, update);
    }
    assert!(matches!(store.finalized_header, LightClientHeader::Gloas(_)));

    let finality: FinalityUpdate<Spec> =
        serde_json::from_value(fixture("finality_update.json")["data"].clone()).unwrap();
    verify_finality_update::<Spec>(&finality, NOW, &store, genesis, &forks).unwrap();
    apply_finality_update::<Spec>(&mut store, &finality);
    assert!(store.finalized_header.beacon().slot > before);
    assert_eq!(
        store.finalized_header.execution_block_hash().unwrap(),
        finality.finalized_header().execution_block_hash().unwrap()
    );
}

#[test]
fn a_tampered_finality_update_is_refused() {
    let forks = forks();
    let genesis = genesis_root();
    let mut store = store_from("bootstrap_fulu.json");
    for u in fixture("updates.json").as_array().unwrap() {
        let update: Update<Spec> = serde_json::from_value(u["data"].clone()).unwrap();
        apply_update::<Spec>(&mut store, &update);
    }
    let good = fixture("finality_update.json")["data"].clone();

    let mut hash = good.clone();
    hash["finalized_header"]["execution_block_hash"] = Value::String(format!("0x{}", "22".repeat(32)));
    let hash: FinalityUpdate<Spec> = serde_json::from_value(hash).unwrap();
    assert!(verify_finality_update::<Spec>(&hash, NOW, &store, genesis, &forks).is_err());

    let mut branch = good.clone();
    branch["finality_branch"][0] = Value::String(format!("0x{}", "33".repeat(32)));
    let branch: FinalityUpdate<Spec> = serde_json::from_value(branch).unwrap();
    assert!(verify_finality_update::<Spec>(&branch, NOW, &store, genesis, &forks).is_err());

    // Signed under the Gloas domain: Fulu's version does not verify it.
    let mut fulu_domain = forks.clone();
    fulu_domain.gloas.fork_version = forks.fulu.fork_version;
    let finality: FinalityUpdate<Spec> = serde_json::from_value(good).unwrap();
    assert!(verify_finality_update::<Spec>(&finality, NOW, &store, genesis, &fulu_domain).is_err());
}
