//! Rollups: blocks or headers their sequencer signed, and tree proofs.

#[cfg(feature = "arbitrum")]
pub mod arbitrum;
#[cfg(feature = "base")]
pub mod base;
#[cfg(feature = "eden")]
pub mod eden;
/// What Base and Arbitrum share.
#[cfg(any(feature = "base", feature = "arbitrum"))]
pub mod sequenced;
