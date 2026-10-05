//! Gathers what the enclave needs to verify Base: a block its sequencer signed, and tree proofs.
//!
//! Base's sequencer signs blocks only on the OP Stack p2p network, so this chain runs a
//! listener on it (`base/gossip.rs`) and attests the newest signed block the RPC can prove.

mod gossip;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tee_node::chains::l2::base::{BASE, TREE_SLOT};
use tee_node::state::IsmState;

use super::sequenced::{Config, Recent, Sequenced, SignedHead, TRIES};
use crate::origin::{Indexer, Message, Step};

/// One listener per process, however many routes start from Base.
static GOSSIP: OnceLock<Arc<Recent<SignedHead>>> = OnceLock::new();

pub struct Base {
    chain: Sequenced,
    port: u16,
}

impl Base {
    pub fn new(config: Config) -> Result<Self> {
        anyhow::ensure!(
            config.domain == BASE.domain,
            "chain `base` has domain {}, but the enclave attests Base as {}",
            config.domain,
            BASE.domain
        );
        Ok(Self {
            chain: Sequenced::new(&BASE, TREE_SLOT, &config),
            port: config.p2p_port,
        })
    }

    /// The listener's newest blocks, starting it on first use: that is inside the runtime,
    /// which building the config is not.
    fn recent(&self) -> &Recent<SignedHead> {
        GOSSIP.get_or_init(|| gossip::start(self.port))
    }
}

#[async_trait]
impl Indexer for Base {
    async fn gather(&self, trusted: &IsmState) -> Result<Step> {
        self.recent().check_alive("base sequencer on p2p")?;
        let heads = self.recent().above(trusted.height, TRIES);
        self.chain.step(trusted, &heads).await
    }

    async fn index(&self, from: u64, to: u64) -> Result<Vec<Message>> {
        self.chain.index(from, to).await
    }

    async fn bootstrap(&self, identity: [u8; 32], _height: Option<u64>) -> Result<IsmState> {
        let head = self.recent().first(Duration::from_secs(300)).await?;
        Ok(self.chain.genesis(&head, identity))
    }
}
