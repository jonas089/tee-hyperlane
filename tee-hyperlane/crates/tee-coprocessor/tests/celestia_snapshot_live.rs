//! The snapshot the enclave replays from has to verify under the ISM's *own* state root, not
//! just under the head's. This walks that path against live Mocha.
//!
//! It is the check the coprocessor used to skip: the Celestia path fetched the proof at the
//! trusted height, threw it away, and sent the decoded tree. Nothing then tied where a batch
//! started to where the ISM stood, which is how a caller could drop selected messages by
//! choosing a snapshot further along.
//!
//! Ignored by default: it reads two historical heights, which public Mocha endpoints prune.
//!
//!   CELESTIA_ARCHIVE_RPC=... cargo test -p tee-coprocessor --test celestia_snapshot_live -- --ignored

use tee_coprocessor::celestia::CelestiaReader;
use tee_node::origins::celestia::{get_celestia_root, CelestiaStore};
use tee_node::state_proofs::get_celestia_merkle_tree;

const HOOK_ID: &str = "0x726f757465725f706f73745f6469737061746368000000030000000000000000";

#[tokio::test]
#[ignore]
async fn a_snapshot_proof_verifies_under_the_height_it_was_read_at() {
    let rpc = std::env::var("CELESTIA_ARCHIVE_RPC")
        .unwrap_or_else(|_| "https://rpc-mocha.pops.one".to_string());
    let reader = CelestiaReader::new(&rpc).unwrap();
    let hook_id: [u8; 32] =
        hex::decode(HOOK_ID.trim_start_matches("0x")).unwrap().try_into().unwrap();

    // Two heights far enough apart that the tree could plausibly have moved between them.
    let head = reader.latest_height().await.unwrap();
    let at = head - 40;
    let earlier = at - 200;

    // An app hash is carried by the *next* header, so a root read at `h` is verified by the
    // header at `h + 1` - the same offset the attest path uses.
    let app_hash = |h: u64| {
        let reader = CelestiaReader::new(&rpc).unwrap();
        async move {
            let block = reader.light_block(h + 1).await.unwrap();
            get_celestia_root(&CelestiaStore { trusted: block }).unwrap().state_root.0
        }
    };

    let (bytes_at, proof_at) = reader.merkle_tree_hook_proof(hook_id, at).await.unwrap();
    let (bytes_earlier, proof_earlier) =
        reader.merkle_tree_hook_proof(hook_id, earlier).await.unwrap();

    let root_at = app_hash(at).await;
    let root_earlier = app_hash(earlier).await;

    // Each proof verifies under its own root. This is what makes `prev_state.state_root` a
    // usable anchor for the snapshot.
    get_celestia_merkle_tree(root_at, hook_id, &bytes_at, &proof_at)
        .expect("the head proof must verify under the head's app hash");
    get_celestia_merkle_tree(root_earlier, hook_id, &bytes_earlier, &proof_earlier)
        .expect("the snapshot proof must verify under the trusted height's app hash");

    // And neither verifies under the other's, which is what stops a caller substituting one.
    assert!(
        get_celestia_merkle_tree(root_at, hook_id, &bytes_earlier, &proof_earlier).is_err(),
        "a proof read at {earlier} must not verify under the app hash at {at}"
    );
    assert!(
        get_celestia_merkle_tree(root_earlier, hook_id, &bytes_at, &proof_at).is_err(),
        "a proof read at {at} must not verify under the app hash at {earlier}"
    );
}
