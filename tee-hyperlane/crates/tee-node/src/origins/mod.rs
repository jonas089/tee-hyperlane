//! Where a verified state root comes from, one origin per file.
//!
//! Every origin answers the same question - "what is the state root at the head, and when
//! was it" - and adding a network means adding one file plus one arm below. Nothing else in
//! the bridge changes: the circuits, the ISM state format and the message path are all
//! origin-agnostic.
//!
//! Two of these origins cost nothing extra to run. Arbitrum and Base publish their L2 state
//! roots *into Ethereum L1 storage*, so once the Ethereum light client has verified an L1
//! state root, those L2 roots are reachable by MPT proof from it - no second light client,
//! no second enclave.

pub mod celestia;
pub mod ethereum;
pub mod ethereum_l2;

use alloy_primitives::B256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Origin {
    Ethereum,
    Celestia,
    Arbitrum,
    Base,
}

impl Origin {
    /// Hyperlane domain ids for the testnets this bridge targets.
    pub fn domain(&self) -> u32 {
        match self {
            Origin::Ethereum => 11155111,
            Origin::Celestia => 1297040200,
            Origin::Arbitrum => 421614,
            Origin::Base => 84532,
        }
    }

    /// Whether this origin rides on another origin's light client rather than its own.
    pub fn is_derived_from_ethereum(&self) -> bool {
        matches!(self, Origin::Arbitrum | Origin::Base)
    }
}

/// A state root the enclave is willing to attest, with the head it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttestedRoot {
    pub state_root: B256,
    pub height: u64,
    /// Head timestamp in seconds; anchors the quote's clock. Must not go backwards.
    pub timestamp: u64,
}
