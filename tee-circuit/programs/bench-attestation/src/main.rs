//! Benchmark guest: the expensive half of attestation, so it can be proved and timed.
//!
//! The production programs additionally bind the attested payload to `report_data` and check
//! the enclave identity. Neither is satisfiable with a borrowed third-party quote - you
//! cannot invert sha256 to make someone else's quote commit to your payload, and their event
//! log names their enclave, not ours. Both are cheap: a sha256 over ~250 bytes and a handful
//! of 48-byte comparisons.
//!
//! What *is* expensive is here and identical to production: DCAP signature-chain
//! verification and the SHA-384 event-log replay. So this measures the real cost.
//!
//! Not part of the bridge. Nothing consumes its proofs.

#![no_main]
sp1_zkvm::entrypoint!(main);

use tee_attestation::{replay_event_logs, verify_quote, AttestationInputs, EventLog};

pub fn main() {
    let inputs: AttestationInputs = sp1_zkvm::io::read();

    let collateral = inputs.decode_collateral().expect("collateral");
    let report = verify_quote(&inputs.quote, &collateral, inputs.now).expect("quote");

    let event_log: Vec<EventLog> = serde_json::from_slice(&inputs.event_log).unwrap_or_default();
    let rtmrs = replay_event_logs(&event_log);

    sp1_zkvm::io::commit_slice(&rtmrs.concat());
    sp1_zkvm::io::commit_slice(&tee_attestation::get_report_data(&report.report));
}
