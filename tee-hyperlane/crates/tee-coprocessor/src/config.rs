//! One TOML file describes every route the coprocessor drives.
//!
//! A route is one direction: messages leaving an origin chain and arriving at a destination
//! chain. Adding a direction is a config block, not code.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// How often to look for work. Interval-driven rather than event-driven, so behaviour
    /// is the same whether the chain is busy or idle.
    #[serde(default = "default_tick_secs")]
    pub tick_secs: u64,
    /// Where finished proofs are kept, so a crash after proving does not discard the work.
    #[serde(default = "default_proof_dir")]
    pub proof_dir: String,
    pub routes: Vec<RouteConfig>,
}

fn default_l2_base_slot() -> u64 {
    151
}

fn default_tick_secs() -> u64 {
    60
}

fn default_proof_dir() -> String {
    "~/.tee-hyperlane/proofs".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    /// Human-readable, used in logs and as the proof-store subdirectory.
    pub name: String,
    pub origin: ChainConfig,
    pub destination: ChainConfig,
    /// The enclave that attests this origin.
    pub tee_node_url: String,
    /// ISM to advance on the destination chain.
    pub ism_id: String,
    /// Origin merkle tree hook, 32 bytes hex.
    pub merkle_tree_address: String,
    /// Optional override for an Ethereum origin's light-client checkpoint. Left unset, it
    /// is derived from the ISM's trusted state each tick, so the route needs no memory of
    /// its own.
    #[serde(default)]
    pub checkpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChainConfig {
    Ethereum {
        domain: u32,
        execution_rpc: String,
        /// Reads at the ISM's trusted height go here when set. A public node serves state
        /// proofs for roughly the last 128 blocks; if the relayer is down longer than that,
        /// the snapshot it must prove is already pruned and the route cannot resume without
        /// an archive node.
        #[serde(default)]
        archive_rpc: Option<String>,
        mailbox: String,
        /// The three fields below describe how to read this chain's outbox, so they are
        /// needed only when it is a route's *origin*. As a destination the coprocessor just
        /// calls the mailbox and the ISM.
        #[serde(default)]
        beacon_rpc: Option<String>,
        #[serde(default)]
        merkle_tree_hook: Option<String>,
        /// Base storage slot of the hook's incremental tree. Deployment-specific: Hyperlane's
        /// canonical Sepolia hook uses 103, celestia-zkevm's testnet deployment uses 151.
        #[serde(default)]
        merkle_tree_base_slot: Option<u64>,
    },
    Celestia {
        domain: u32,
        rpc: String,
        /// Historical state proofs and tx search at the ISM's trusted height. See
        /// `archive_rpc` above; Celestia RPCs prune both.
        #[serde(default)]
        archive_rpc: Option<String>,
        grpc: String,
        mailbox_id: String,
        merkle_tree_hook_id: String,
    },
    /// Rides on an Ethereum light client rather than its own.
    EthereumL2 {
        domain: u32,
        /// Must serve state at the *confirmed* L2 block, which is thousands of blocks behind
        /// head. Public nodes have pruned it, so this is an archive endpoint.
        l2_rpc: String,
        /// The Ethereum chain whose light client secures this one.
        l1: Box<ChainConfig>,
        /// Which rollup this is: `arbitrum` or `base`. They prove different things.
        rollup: String,
        /// Arbitrum's BoLD RollupCore, or Base's AnchorStateRegistry.
        l1_anchor_contract: String,
        mailbox: String,
        merkle_tree_hook: String,
        /// Both L2s' hooks use 151; Sepolia's canonical one uses 103.
        #[serde(default = "default_l2_base_slot")]
        merkle_tree_base_slot: u64,
    },
}

impl ChainConfig {
    pub fn domain(&self) -> u32 {
        match self {
            ChainConfig::Ethereum { domain, .. }
            | ChainConfig::Celestia { domain, .. }
            | ChainConfig::EthereumL2 { domain, .. } => *domain,
        }
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }
}
