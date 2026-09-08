//! Run a live enclave's quote through the exact check the guests run, natively.
//!
//! The zkVM does not change *what* is verified - `verify_attestation` is the same function
//! the guest calls - so this answers "will a proof from this enclave be accepted" in
//! milliseconds instead of the tens of minutes a real proof costs, and without the mock
//! prover's silence. Worth running before creating ISMs, whose vkeys are immutable once set:
//! an identity no enclave can satisfy would otherwise surface an hour into the first proof.
//!
//!   ATTESTATION=/path/from/attest-celestia.json \
//!     cargo test -p tee-coprocessor --test preflight_live -- --ignored --nocapture

use tee_attestation::{build_identity_policy, verify_attestation, AttestationInputs};

#[tokio::test]
#[ignore]
async fn a_live_quote_satisfies_the_pinned_identity() {
    let path = std::env::var("ATTESTATION").expect("set ATTESTATION");
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("attestation file")).unwrap();
    let att = &record["attestation"];

    let quote_hex = att["quote"].as_str().expect("quote");
    let quote = hex::decode(quote_hex).unwrap();
    let payload = hex::decode(att["payload"].as_str().expect("payload")).unwrap();
    let event_log = att["event_log"].as_str().expect("event_log").as_bytes().to_vec();

    let collateral = tee_coprocessor::enclave::fetch_collateral(quote_hex).await.unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let inputs = AttestationInputs {
        quote,
        event_log,
        collateral: AttestationInputs::encode_collateral(&collateral),
        now,
        payload,
    };

    let update = verify_attestation(&inputs, build_identity_policy())
        .expect("the pinned identity must accept a quote from the live enclave");
    println!(
        "accepted: origin {} height {} -> {}, {} message ids",
        update.new_state.origin_domain,
        update.prev_state.height,
        update.new_state.height,
        update.message_ids.len()
    );
}
