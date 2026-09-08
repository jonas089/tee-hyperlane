//! Shared, I/O-free logic for the TEE ISM bridge.
//!
//! Three concerns, one module each:
//!
//! * [`attestation`] - is this a genuine TDX quote, and does it say what it claims?
//! * [`enclave_identity`] - is it *our* enclave, on a platform Intel still trusts?
//! * [`ism`] - the byte formats the destination chain stores and reads.
//!
//! Linked by both SP1 guests, by the enclave and by the coprocessor, so that the bytes an
//! enclave attests to are the bytes a circuit commits to are the bytes a chain verifies.

pub mod attestation;
pub mod enclave_identity;
pub mod ism;

pub use attestation::{
    event_digest_matches, get_event_value, get_measurements, get_report_data, replay_event_logs,
    verify_attestation, verify_attested_report, verify_quote, AttestationError, AttestationInputs,
    EventLog, Measurements, APPLICATION_IMR, DSTACK_RUNTIME_EVENT_TYPE, MAX_QUOTE_SKEW_SECS,
};
pub use enclave_identity::{
    build_identity_digest, build_identity_policy, verify_enclave_identity, verify_platform_tcb,
    EnclaveIdentity, IdentityError, IdentityPolicy, ALLOWED_TCB_STATUS,
};
pub use ism::{
    decode_attested_update, decode_ism_state, decode_state_membership_values,
    decode_state_transition_values, encode_attested_update, encode_ism_state,
    encode_state_membership_values, encode_state_transition_values, hash_attested_update,
    verify_transition, AttestedUpdate, IsmState, StateMembershipValues, StateTransitionValues,
    TransitionError, ISM_STATE_BYTES,
};

/// Failures decoding the byte formats this crate defines or mirrors.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("expected {expected} bytes, got {got}")]
    WrongLength { expected: usize, got: usize },
    #[error("input too short: needed {needed} more bytes")]
    TooShort { needed: usize },
    #[error("trailing bytes: {count} unread")]
    TrailingBytes { count: usize },
    #[error("state length {0} is outside the range x/zkism accepts (32..=2048)")]
    StateLengthOutOfRange(u64),
    #[error("message id count {0} exceeds the x/zkism maximum")]
    MessageIdCountTooLarge(u64),
}
