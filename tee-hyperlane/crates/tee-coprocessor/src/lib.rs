//! The coprocessor: everything the bridge does that does not need to be trusted.
//!
//! It gathers data, asks the enclave to attest it, proves the attestation on local CPU, and
//! relays the result. None of that is privileged - a dishonest coprocessor can stall the
//! bridge, but it cannot make either chain accept a message the enclave did not attest.

pub mod api;
pub mod celestia;
pub mod chains;
pub mod commands;
pub mod enclave;
pub mod ethereum;
pub mod config;
pub mod tasks;
