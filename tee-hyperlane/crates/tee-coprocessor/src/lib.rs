//! The coprocessor: everything the bridge does that does not need to be trusted.
//!
//! It finds what the enclave needs, asks it to attest, and delivers the result. A dishonest
//! coprocessor can stall the bridge; it cannot make a chain accept a message the enclave did
//! not attest.
//!
//! One file per chain under the chain it rides on, mirroring the enclave: `ethereum/base.rs`
//! finds what the enclave's `ethereum/base.rs` verifies.

pub mod api;
pub mod config;
pub mod destination;
pub mod evm;
pub mod identity;
pub mod origin;
pub mod route;

/// Celestia, and Eden, which rides on it.
pub mod celestia;
/// Ethereum, and Arbitrum and Base, which ride on it.
pub mod ethereum;
