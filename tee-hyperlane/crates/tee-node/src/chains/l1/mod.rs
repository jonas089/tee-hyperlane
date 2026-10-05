//! Chains with their own consensus, verified by a light client.

#[cfg(feature = "celestia")]
pub mod celestia;
#[cfg(feature = "ethereum")]
pub mod ethereum;
