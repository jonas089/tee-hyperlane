//! The coprocessor: everything the bridge does that does not need to be trusted.
//!
//! It finds what the enclave needs, asks it to attest, and delivers the result. A dishonest
//! coprocessor can stall the bridge; it cannot make a chain accept a message the enclave did
//! not attest.
//!
//! Two directories, one per side of a route. `origin/` holds the `Indexer` trait and one file
//! per chain that implements it, under the chain it rides on and mirroring the enclave:
//! `origin/ethereum/base.rs` finds what the enclave's `ethereum/base.rs` verifies.
//! `destination/` holds the `Destination` trait and its two implementations, EVM and Celestia.

pub mod api;
pub mod config;
pub mod destination;
pub mod identity;
pub mod origin;
pub mod route;
