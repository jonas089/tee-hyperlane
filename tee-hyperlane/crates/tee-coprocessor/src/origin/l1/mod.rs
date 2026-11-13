//! Chains with their own consensus: light-client updates and tree proofs.

#[cfg(feature = "celestia")]
pub mod celestia;
#[cfg(feature = "ethereum")]
pub mod ethereum;
