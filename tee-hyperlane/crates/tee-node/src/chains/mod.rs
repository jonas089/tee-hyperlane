//! The origins, one file each.
//!
//! * `l1/` - chains with their own consensus, verified by a light client.
//! * `l2/` - rollups, verified by their sequencer's signature over a block or header.

pub mod l1;
pub mod l2;
