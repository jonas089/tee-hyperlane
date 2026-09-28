//! The enclave: a stateless verifier of origin-chain state.
//!
//! It is handed everything it needs, verifies all of it, attests to the result, and forgets.
//! No disk, no keys, no outbound network.
//!
//! One file per chain, under the chain it rides on: `ethereum/arbitrum.rs`, `celestia/eden.rs`.
//! Each image compiles only its own family's files, so a change to one chain moves only that
//! family's enclave identity. `attest.rs`, `origin.rs`, `state.rs` and `evm.rs` are shared by
//! all of them.

pub mod attest;
pub mod evm;
pub mod origin;
pub mod state;

/// Celestia, and Eden, which rides on it.
#[cfg(feature = "celestia")]
pub mod celestia;
/// Ethereum, and Arbitrum and Base, which ride on it.
#[cfg(feature = "ethereum")]
pub mod ethereum;
