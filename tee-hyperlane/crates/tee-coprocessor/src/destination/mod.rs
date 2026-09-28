//! Where a route delivers: read the ISM's state, submit an attestation, deliver its messages.
//!
//! Two kinds, EVM chains and Celestia. Both sign through the tools that deploy them - `cast`
//! and `celestia-appd` - rather than reimplementing signing: the relayer key is the least
//! sensitive thing in the system, since it can pay gas and stall a route but cannot make a
//! chain accept a message the enclave did not attest.
//!
//! Submitting is idempotent, so a batch interrupted anywhere can simply be submitted again:
//! an ISM already at the batch's state is not advanced twice, and a message already delivered
//! is skipped.

use anyhow::{Context, Result};
use async_trait::async_trait;
use tee_node::state::IsmState;
use tokio::process::Command;

#[async_trait]
pub trait Destination: Send + Sync {
    /// The ISM's trusted state: the only progress marker a route has, and it lives on chain,
    /// which is what makes restarting the same as continuing.
    async fn state(&self) -> Result<IsmState>;
    async fn submit(&self, batch: &Batch) -> Result<()>;
}

/// An attested batch, as the route stages it.
pub struct Batch {
    pub quote: String,
    pub event_log: String,
    /// The attested payload: `prev_state(116) new_state(116) tree(32) attested_at(8) count(8) ids`.
    pub payload: Vec<u8>,
    /// The message bytes, in tree order.
    pub messages: Vec<Vec<u8>>,
}

/// The ISM has moved past the state this batch starts from, so no retry can land it. The route
/// discards the batch and builds a new one from where the ISM actually is.
#[derive(Debug, thiserror::Error)]
#[error("stale batch: the ISM has moved past the state this batch starts from")]
pub struct Stale;

impl Batch {
    fn prev_state(&self) -> &[u8] {
        &self.payload[..116]
    }

    fn new_state(&self) -> &[u8] {
        &self.payload[116..232]
    }

    /// The messages addressed to `domain`, with their ids.
    fn for_domain(&self, domain: u32) -> impl Iterator<Item = (String, &Vec<u8>)> {
        self.messages.iter().filter_map(move |m| {
            let decoded = hyperlane_types::decode_hyperlane_message(m).ok()?;
            (decoded.destination == domain)
                .then(|| (hex::encode(alloy_primitives::keccak256(m)), m))
        })
    }

    /// Refuse a batch the ISM has moved past. `true` when the ISM is already at its new state,
    /// so only delivery is left to do.
    fn advanced(&self, state: &[u8]) -> Result<bool> {
        if state == self.new_state() {
            return Ok(true);
        }
        anyhow::ensure!(state == self.prev_state(), Stale);
        Ok(false)
    }
}

/// Run one of the signing tools and return its trimmed stdout, or its stderr as the error.
async fn run(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{program} {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

mod celestia;
mod evm;

pub use celestia::Celestia;
pub use evm::Evm;
