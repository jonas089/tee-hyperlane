//! Reading Celestia, so the enclave can verify it.
//!
//! Nothing gathered here is trusted. Light blocks are re-verified inside the enclave against
//! the header the ISM already trusts, and the merkle tree is re-proven against the app hash
//! that verification produces. A lying RPC yields a rejected attestation, not a wrong root.

use anyhow::{Context, Result};
use tee_node::state_proofs::{get_merkle_tree_hook_key, StoreProofOp};
use tendermint::block::Height;
use tendermint_light_client_verifier::types::LightBlock;
use tendermint_rpc::query::Query;
use tendermint_rpc::{Client, HttpClient, Order, Paging};
use tracing::debug;

/// One Hyperlane message as the origin chain recorded it, in tree-insert order.
/// Heights per `tx_search` call. Small enough that any one window stays inside a public
/// node's response limits, large enough that catching up does not take thousands of requests.
const HEIGHT_WINDOW: u64 = 500;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DispatchedMessage {
    pub height: u64,
    pub tree_index: u32,
    pub message_id: [u8; 32],
    /// The raw Hyperlane message, needed to deliver it on the destination.
    pub message: Vec<u8>,
}

pub struct CelestiaReader {
    rpc: HttpClient,
}

impl CelestiaReader {
    pub fn new(rpc_url: &str) -> Result<Self> {
        Ok(Self {
            rpc: HttpClient::new(rpc_url).context("celestia rpc")?,
        })
    }

    pub async fn latest_height(&self) -> Result<u64> {
        Ok(self
            .rpc
            .status()
            .await?
            .sync_info
            .latest_block_height
            .value())
    }

    /// Assemble the light block at `height`.
    ///
    /// Carries the validator set that signed this header *and* the one expected to sign the
    /// next, because that pair is what lets a light client step forward from here.
    pub async fn light_block(&self, height: u64) -> Result<LightBlock> {
        let h = Height::try_from(height)?;
        let commit = self.rpc.commit(h).await?;
        let validators = self.rpc.validators(h, Paging::All).await?;
        let next = self
            .rpc
            .validators(Height::try_from(height + 1)?, Paging::All)
            .await?;
        let peer_id = self.rpc.status().await?.node_info.id;
        Ok(LightBlock::new(
            commit.signed_header,
            tendermint::validator::Set::new(validators.validators, None),
            tendermint::validator::Set::new(next.validators, None),
            peer_id,
        ))
    }

    /// Prove the merkle tree hook out of application state at `height`.
    ///
    /// The app hash committing to this lives in the header at `height + 1`, which is the
    /// header the enclave verifies.
    pub async fn merkle_tree_hook_proof(
        &self,
        hook_id: [u8; 32],
        height: u64,
    ) -> Result<(Vec<u8>, Vec<StoreProofOp>)> {
        let key = get_merkle_tree_hook_key(hook_id);
        let response = self
            .rpc
            .abci_query(
                Some("/store/hyperlane/key".to_string()),
                key,
                Some(Height::try_from(height)?),
                true,
            )
            .await
            .context("abci_query for the merkle tree hook")?;

        anyhow::ensure!(
            response.height.value() == height,
            "node answered at height {} rather than {height}",
            response.height
        );
        let ops = response
            .proof
            .context("node returned no proof; it may not have `prove` enabled")?
            .ops
            .into_iter()
            .map(|op| StoreProofOp {
                proof_type: op.field_type,
                key: op.key,
                data: op.data,
            })
            .collect();
        Ok((response.value, ops))
    }

    /// Find the messages inserted into the merkle tree between two heights.
    ///
    /// Uses the tx indexer rather than `block_results`: most public Celestia nodes run with
    /// `discard_abci_responses = true` and refuse the latter outright.
    ///
    /// Insert order matters, not dispatch order. The tree is replayed leaf by leaf, so a
    /// message hyperlane-cosmos inserted twice must be reported twice, or the replayed
    /// branch will not match the chain's.
    pub async fn dispatched_messages(
        &self,
        from_height: u64,
        to_height: u64,
    ) -> Result<Vec<DispatchedMessage>> {
        // Scanned in windows, because the height range is unbounded in practice.
        //
        // `tx.height >= a AND tx.height <= b` matches every transaction on the chain in that
        // span, not just Hyperlane's. A route that had been idle for four days asked for
        // 64,000 blocks, which is 120,000 transactions, which is 1,200 pages - and the search
        // simply never returned, so the route could not catch up and stayed stuck. Windowing
        // bounds the work per request no matter how far behind a route has fallen; the total
        // is still proportional to the gap, but each step now completes.
        let mut out = Vec::new();
        let mut start = from_height;
        while start <= to_height {
            let end = (start + HEIGHT_WINDOW - 1).min(to_height);
            let query: Query = format!("tx.height >= {start} AND tx.height <= {end}").parse()?;
            let mut page = 1u32;
            loop {
                // Retried per window rather than per sweep. Catching up across days is over a
                // hundred requests to a public node, and one flaky answer used to discard the
                // whole sweep and start again next tick - which, for a route far enough
                // behind, means it never finishes at all.
                let results = {
                    let mut attempt = 0;
                    loop {
                        match self
                            .rpc
                            .tx_search(query.clone(), false, page, 100, Order::Ascending)
                            .await
                        {
                            Ok(results) => break results,
                            Err(e) if attempt < 4 => {
                                attempt += 1;
                                tokio::time::sleep(std::time::Duration::from_millis(250 * attempt))
                                    .await;
                                debug!(window = %format!("{start}..={end}"), attempt, error = %e,
                                       "retrying tx_search");
                            }
                            Err(e) => {
                                return Err(e)
                                    .with_context(|| format!("tx_search over {start}..={end}"))
                            }
                        }
                    }
                };
                if results.txs.is_empty() {
                    break;
                }
                for tx in &results.txs {
                    out.extend(inserts_in(tx.height.value(), &tx.tx_result.events)?);
                }
                if results.txs.len() < 100 {
                    break;
                }
                page += 1;
            }
            start = end + 1;
        }
        out.sort_by_key(|m| m.tree_index);
        Ok(out)
    }
}

/// Pair each tree insertion with the dispatch it came from, matching on message id.
fn inserts_in(height: u64, events: &[tendermint::abci::Event]) -> Result<Vec<DispatchedMessage>> {
    let mut dispatched: Vec<Vec<u8>> = Vec::new();
    let mut inserts: Vec<(u32, [u8; 32])> = Vec::new();

    for ev in events {
        match ev.kind.as_str() {
            "hyperlane.core.v1.EventDispatch" => {
                if let Some(raw) = attribute(ev, "message") {
                    dispatched.push(decode_hex(&raw)?);
                }
            }
            "hyperlane.core.post_dispatch.v1.EventInsertedIntoTree" => {
                let index: u32 = attribute(ev, "index")
                    .context("insert event without index")?
                    .parse()?;
                let id =
                    decode_hex(&attribute(ev, "message_id").context("insert event without id")?)?;
                inserts.push((
                    index,
                    id.as_slice().try_into().context("message id length")?,
                ));
            }
            _ => {}
        }
    }

    inserts
        .into_iter()
        .map(|(tree_index, message_id)| {
            let message = dispatched
                .iter()
                .find(|m| {
                    hyperlane_types::decode_hyperlane_message(m)
                        .map(|d| hyperlane_types::get_message_id(&d) == message_id)
                        .unwrap_or(false)
                })
                .cloned()
                .unwrap_or_default();
            DispatchedMessage {
                height,
                tree_index,
                message_id,
                message,
            }
        })
        .map(Ok)
        .collect()
}

fn attribute(ev: &tendermint::abci::Event, key: &str) -> Option<String> {
    ev.attributes.iter().find_map(|a| {
        (a.key_str().ok()? == key)
            .then(|| a.value_str().ok().map(|v| v.trim_matches('"').to_string()))?
    })
}

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    Ok(hex::decode(s.trim_start_matches("0x"))?)
}
