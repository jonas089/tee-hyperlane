//! The enclave: a stateless verifier of origin-chain state.
//!
//! It is handed everything it needs, verifies all of it, attests to the result, and forgets.
//! No disk, no keys, no outbound network.
//!
//! `chains/` holds one file per origin and one cargo feature per origin, so each image compiles
//! only its own chain. Everything else here is shared by all of them.

pub mod attest;
pub mod chains;
pub mod evm;
pub mod origin;
pub mod state;
