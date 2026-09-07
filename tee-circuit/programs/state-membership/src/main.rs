//! Proves: the same enclave attestation also authorises this batch of Hyperlane messages.
//!
//! Commits it in the shape `x/zkism`'s MsgSubmitMessages reads. This is a second proof of
//! the *same* quote rather than a separate inclusion circuit: the enclave already verified
//! inclusion against the root it attested, and an on-chain inclusion proof under a
//! TEE-attested root adds nothing over the enclave that produced the root.

#![no_main]
sp1_zkvm::entrypoint!(main);

use tee_attestation::{
    build_identity_policy, encode_state_membership_values, verify_attestation, AttestationInputs,
};

pub fn main() {
    let inputs: AttestationInputs = sp1_zkvm::io::read();

    let update = verify_attestation(&inputs, build_identity_policy()).expect("attestation rejected");

    sp1_zkvm::io::commit_slice(&encode_state_membership_values(
        update.new_state.state_root,
        update.merkle_tree_address,
        &update.message_ids,
    ));
}
