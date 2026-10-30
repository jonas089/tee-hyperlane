//! The `Destination` trait: read an ISM's state, submit an attestation, and deliver messages.

use anyhow::{Context, Result};
use async_trait::async_trait;
use tee_node::state::IsmState;
use tokio::process::Command;

#[async_trait]
pub trait Destination: Send + Sync {
    /// The ISM's trusted state: the only progress marker a route has, and it lives on chain,
    /// which is what makes restarting the same as continuing.
    async fn state(&self) -> Result<IsmState>;
    /// Land the batch's attestation if the ISM is not already past it, then deliver every
    /// message for this destination.
    async fn submit(&self, batch: &Batch) -> Result<Submitted>;
    /// Deliver one message an attested root covers, with the metadata that proves it: the root,
    /// the message's index in the origin tree and its merkle branch.
    async fn deliver(&self, message: &[u8], metadata: &[u8]) -> Result<Outcome>;
    /// Whether the mailbox has processed message `id` (hex, no prefix), by anyone.
    async fn delivered(&self, id: &str) -> Result<bool>;
    /// The origin tree the ISM last attested: its root and leaf count. A zero root before the
    /// first attestation.
    async fn tree(&self) -> Result<([u8; 32], u32)>;
    /// Whether the ISM has ever attested `root`. Every root it has stays accepted, so a proof
    /// against one lands whenever it is sent; a proof against any other root never does.
    async fn root_attested(&self, root: [u8; 32]) -> Result<bool>;
    /// The relayer's own account on this chain and its balance of the gas token, in base units.
    async fn wallet(&self) -> Result<Wallet>;
}

/// The account that pays this destination's gas.
#[derive(Debug, Clone)]
pub struct Wallet {
    pub address: String,
    pub balance: u128,
}

/// What submitting a batch did.
#[derive(Debug, Clone)]
pub struct Submitted {
    /// The attestation's own transaction. `None` when the ISM was already past the batch, as
    /// when a restart finds an attestation that landed before the process stopped.
    pub attestation_tx: Option<String>,
    pub deliveries: Vec<Delivery>,
}

/// What became of one message of a batch.
#[derive(Debug, Clone)]
pub struct Delivery {
    /// The message id, hex without a prefix.
    pub id: String,
    pub message: Vec<u8>,
    /// What proves the message against an attested root; kept with it so a retry needs nothing
    /// recomputed.
    pub metadata: Vec<u8>,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Processed by this transaction.
    Delivered { tx: String },
    /// Already processed, by an earlier attempt or someone else.
    AlreadyDelivered,
    /// The destination refused this message; `tx` is the failed transaction, if one landed.
    Refused { tx: Option<String>, reason: String },
}

/// An attested step, as the route stages it: the attestation, and the messages it lets through
/// with the metadata each proves itself with.
pub struct Batch {
    pub quote: String,
    pub event_log: String,
    /// The attested payload: `prev_state(116) new_state(116) tree(32) attested_at(8) tree_root(32) tree_count(4)`.
    pub payload: Vec<u8>,
    /// The message bytes, in tree order.
    pub messages: Vec<Vec<u8>>,
    /// Per message, same order: `root(32) index(4) proof(32 * 32)` against the attested root.
    pub metadata: Vec<Vec<u8>>,
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

    pub(crate) fn new_state(&self) -> &[u8] {
        &self.payload[116..232]
    }

    /// The messages addressed to `domain`, with their ids and metadata.
    pub(crate) fn for_domain(
        &self,
        domain: u32,
    ) -> impl Iterator<Item = (String, &Vec<u8>, &Vec<u8>)> {
        self.messages
            .iter()
            .zip(self.metadata.iter())
            .filter_map(move |(m, meta)| {
                let decoded = hyperlane_types::decode_hyperlane_message(m).ok()?;
                (decoded.destination == domain)
                    .then(|| (hex::encode(alloy_primitives::keccak256(m)), m, meta))
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

/// The longest any one tool call may take. `cast receipt` waits for inclusion, which on the
/// slowest destination is well under a minute; a call past this has hung, and a hung call would
/// otherwise stall its route with nothing in the log.
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Run one of the signing tools and return its trimmed stdout, or its stderr as the error.
async fn run(program: &str, args: &[&str]) -> Result<String> {
    run_for(program, args, TOOL_TIMEOUT).await
}

async fn run_for(program: &str, args: &[&str], limit: std::time::Duration) -> Result<String> {
    let child = Command::new(program).args(args).kill_on_drop(true).output();
    let out = tokio::time::timeout(limit, child)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "{program} {} did not finish within {}s",
                args.first().unwrap_or(&""),
                limit.as_secs()
            )
        })?
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{program} {}: {}",
            args.first().unwrap_or(&""),
            crate::brief(&String::from_utf8_lossy(&out.stderr))
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_hung_tool_is_an_error_not_a_stall() {
        let started = std::time::Instant::now();
        let e = run_for("sleep", &["30"], std::time::Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("did not finish"), "{e}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}

mod celestia;
mod evm;

pub use celestia::Celestia;
pub use evm::Evm;
