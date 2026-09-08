//! Assembles a real Base root proof and checks the enclave's verifier accepts it.
//!
//! Ignored by default: it needs an archive endpoint. Base's anchor trails the head by a full
//! dispute window, which on Sepolia is about five days, so no public node keeps that state.
//!
//!   ALCHEMY_API_KEY=... cargo test -p tee-coprocessor --test base_live -- --ignored

use tee_coprocessor::ethereum::ExecutionReader;
use tee_coprocessor::ethereum_l2::get_base_root_proof;
use tee_node::origins::ethereum_l2::verify_base_root;

const ANCHOR_STATE_REGISTRY: &str = "0x2fF5cC82dBf333Ea30D8ee462178ab1707315355";

#[tokio::test]
#[ignore]
async fn a_live_anchor_game_yields_bases_state_root() {
    let key = std::env::var("ALCHEMY_API_KEY").expect("ALCHEMY_API_KEY");
    let l1 = ExecutionReader::new("https://rpc.sepolia.ethpandaops.io");
    let l2 = ExecutionReader::new(&format!("https://base-sepolia.g.alchemy.com/v2/{key}"));

    let head: u64 = u64::from_str_radix(
        l1.call("eth_blockNumber", serde_json::json!([]))
            .await
            .unwrap()
            .as_str()
            .unwrap()
            .trim_start_matches("0x"),
        16,
    )
    .unwrap();
    let l1_block = head - 8;

    let proof = get_base_root_proof(&l1, &l2, ANCHOR_STATE_REGISTRY.parse().unwrap(), l1_block)
        .await
        .expect("gathering the proof");

    let l1_state_root: alloy_primitives::B256 = l1
        .call(
            "eth_getBlockByNumber",
            serde_json::json!([format!("0x{l1_block:x}"), false]),
        )
        .await
        .unwrap()["stateRoot"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // The same check the enclave makes.
    let root = verify_base_root(l1_state_root, &proof).expect("deriving the root");
    println!(
        "base block {} root {} at {}",
        root.height, root.state_root, root.timestamp
    );
    assert!(root.height > 0);
    assert_ne!(root.state_root, alloy_primitives::B256::ZERO);
}
