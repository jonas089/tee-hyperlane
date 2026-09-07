//! The enclave: a stateless verifier of origin-chain state.
//!
//! It is handed everything it needs, verifies all of it, attests to the result, and forgets.
//! No disk, no keys, no outbound network.

pub mod attest;
pub mod dstack;
pub mod hyperlane_state;
pub mod origins;
pub mod state_proofs;
