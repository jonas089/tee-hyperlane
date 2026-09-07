//! Proves: an enclave this build accepts attested a state transition.
//!
//! Commits it in the shape `x/zkism`'s MsgUpdateInterchainSecurityModule reads.

#![no_main]
sp1_zkvm::entrypoint!(main);

use tee_attestation::{
    build_identity_policy, encode_ism_state, encode_state_transition_values, verify_attestation,
    AttestationInputs,
};

pub fn main() {
    let inputs: AttestationInputs = sp1_zkvm::io::read();

    let update = verify_attestation(&inputs, build_identity_policy()).expect("attestation rejected");

    sp1_zkvm::io::commit_slice(&encode_state_transition_values(
        &encode_ism_state(&update.prev_state),
        &encode_ism_state(&update.new_state),
    ));
}
