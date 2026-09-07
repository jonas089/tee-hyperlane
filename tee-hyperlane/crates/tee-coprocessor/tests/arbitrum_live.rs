//! Assembles a real Arbitrum root proof and checks the enclave's verifier accepts it.
//!
//! Ignored by default: it needs an archive endpoint, because the confirmed assertion is
//! thousands of L2 blocks behind head and public nodes have pruned that state.
//!
//!   ALCHEMY_API_KEY=... cargo test -p tee-coprocessor --test arbitrum_live -- --ignored

use tee_coprocessor::arbitrum::get_arbitrum_root_proof;
use tee_coprocessor::ethereum::ExecutionReader;
use tee_node::origins::ethereum_l2::{get_arbitrum_root, RollupLayout};

const ROLLUP: &str = "0x042B2E6C5E99d4c521bd49beeD5E99651D9B0Cf4";

#[tokio::test]
#[ignore]
async fn a_live_confirmed_assertion_yields_arbitrums_state_root() {
    let key = std::env::var("ALCHEMY_API_KEY").expect("ALCHEMY_API_KEY");
    let l1 = ExecutionReader::new("https://rpc.sepolia.ethpandaops.io");
    let l2 = ExecutionReader::new(&format!("https://arb-sepolia.g.alchemy.com/v2/{key}"));

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

    let proof =
        get_arbitrum_root_proof(&l1, &l2, ROLLUP.parse().unwrap(), RollupLayout::ARBITRUM_SEPOLIA, l1_block)
            .await
            .expect("gathering the proof");

    let l1_state_root: alloy_primitives::B256 = l1
        .call("eth_getBlockByNumber", serde_json::json!([format!("0x{l1_block:x}"), false]))
        .await
        .unwrap()["stateRoot"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // The same check the enclave makes.
    let root = get_arbitrum_root(l1_state_root, &proof).expect("deriving the root");
    println!("arbitrum block {} root {} at {}", root.height, root.state_root, root.timestamp);
    assert!(root.height > 0);
    assert_ne!(root.state_root, alloy_primitives::B256::ZERO);
}
