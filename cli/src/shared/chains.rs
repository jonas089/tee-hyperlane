//! Every chain this bridge touches, and the fixed facts about each. Anything an operator might
//! want to point elsewhere (an endpoint) can be overridden from devnet/.env by the name given.

use alloy::primitives::{address, Address};
use anyhow::{bail, Result};

use crate::shared::settings::Ctx;

/// The bridge's own Celestia chain.
pub mod celestia {
    pub const CHAIN_ID: &str = "teeism-local";
    /// Deliberately not Mocha's: EVM routers are enrolled by domain, and a reused live domain
    /// would let a router here be confused with a testnet one.
    pub const DOMAIN: u32 = 1297040299;
    pub const DENOM: &str = "utia";
    /// What the devnet node charges per gas (`teeism celestia up` sets its minimum). The fee
    /// is the gas limit at this price.
    pub const GAS_PRICES: &str = "0.002utia";
    pub const GAS: &str = "900000";
}

pub fn celestia_rpc(ctx: &Ctx) -> String {
    ctx.var_or("CELESTIA_RPC", "http://localhost:26657")
}

/// A Hyperlane deployment someone else runs, which ours dispatches and delivers through.
#[derive(Clone, Copy)]
pub struct Canonical {
    pub mailbox: Address,
    pub merkle_hook: Address,
}

#[derive(Clone, Copy)]
pub struct EvmChain {
    /// The name the coprocessor config and the UI use.
    pub name: &'static str,
    pub label: &'static str,
    pub chain_id: u64,
    /// Each chain's Hyperlane domain is its chain id.
    pub domain: u32,
    /// The endpoint transactions are sent through: the first of these settings that is set,
    /// else `rpc_default`. The same endpoint the relayer delivers through.
    pub rpc_vars: &'static [&'static str],
    pub rpc_default: &'static str,
    /// None where there is no canonical Hyperlane: we deploy the mailbox (`teeism mailbox`).
    pub canonical: Option<Canonical>,
    /// The enclave that attests this chain as an origin.
    pub enclave: &'static str,
    /// Where the relayer reads it as an origin, for the coprocessor config's `kind`.
    pub kind: &'static str,
}

pub const EVM_CHAINS: [EvmChain; 4] = [
    EvmChain {
        name: "sepolia",
        label: "Ethereum Sepolia",
        chain_id: 11155111,
        domain: 11155111,
        rpc_vars: &["SEPOLIA_SEND_RPC", "SEPOLIA_RPC"],
        rpc_default: "https://ethereum-sepolia-rpc.publicnode.com",
        canonical: Some(Canonical {
            mailbox: address!("fFAEF09B3cd11D9b20d1a19bECca54EEC2884766"),
            merkle_hook: address!("4917a9746A7B6E0A57159cCb7F5a6744247f2d0d"),
        }),
        enclave: "ethereum",
        kind: "ethereum",
    },
    EvmChain {
        name: "arbitrum",
        label: "Arbitrum Sepolia",
        chain_id: 421614,
        domain: 421614,
        rpc_vars: &["ARBITRUM_RPC"],
        rpc_default: "https://sepolia-rollup.arbitrum.io/rpc",
        canonical: Some(Canonical {
            mailbox: address!("598facE78a4302f11E3de0bee1894Da0b2Cb71F8"),
            merkle_hook: address!("AD34A66Bf6dB18E858F6B686557075568c6E031C"),
        }),
        enclave: "arbitrum",
        kind: "arbitrum",
    },
    EvmChain {
        name: "base",
        label: "Base Sepolia",
        chain_id: 84532,
        domain: 84532,
        rpc_vars: &["BASE_RPC"],
        rpc_default: "https://sepolia.base.org",
        canonical: Some(Canonical {
            mailbox: address!("6966b0E55883d49BFB24539356a2f8A673E02039"),
            merkle_hook: address!("86fb9F1c124fB20ff130C41a79a432F770f67AFD"),
        }),
        enclave: "base",
        kind: "base",
    },
    EvmChain {
        name: "eden",
        label: "Eden",
        chain_id: 3735928814,
        domain: 3735928814,
        rpc_vars: &["EDEN_RPC"],
        rpc_default: "https://rpc.testnet.eden.gateway.fm/",
        canonical: None,
        enclave: "eden",
        kind: "eden",
    },
];

impl EvmChain {
    pub fn rpc(&self, ctx: &Ctx) -> String {
        self.rpc_vars
            .iter()
            .find_map(|v| ctx.var(v))
            .unwrap_or_else(|| self.rpc_default.to_string())
    }
}

pub fn evm(name: &str) -> Result<&'static EvmChain> {
    match EVM_CHAINS.iter().find(|c| c.name == name) {
        Some(c) => Ok(c),
        None => bail!(
            "unknown EVM chain '{name}'; the chains are: {}",
            EVM_CHAINS.iter().map(|c| c.name).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// The chains a command covers: the ones named, or all of them.
pub fn evm_selection(named: &[String]) -> Result<Vec<&'static EvmChain>> {
    if named.is_empty() {
        return Ok(EVM_CHAINS.iter().collect());
    }
    named.iter().map(|n| evm(n)).collect()
}

/// One image per origin chain, each answering on its own port, in the shared CVM or alone.
pub const ENCLAVES: [(&str, u16); 5] = [
    ("celestia", 8080),
    ("ethereum", 8081),
    ("base", 8082),
    ("arbitrum", 8083),
    ("eden", 8084),
];

pub fn enclave_port(chain: &str) -> Result<u16> {
    match ENCLAVES.iter().find(|(c, _)| *c == chain) {
        Some((_, p)) => Ok(*p),
        None => bail!(
            "no enclave image for '{chain}'; the images are: {}",
            ENCLAVES.iter().map(|(c, _)| *c).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// The enclave that attests an origin, by the coprocessor's chain name.
pub fn enclave_for(origin: &str) -> &str {
    match origin {
        "celestia" => "celestia",
        other => EVM_CHAINS
            .iter()
            .find(|c| c.name == other)
            .map(|c| c.enclave)
            .unwrap_or(other),
    }
}

/// Where an enclave answers: its CVM's app id and port under the gateway of whichever Phala node
/// runs it. Only plumbing: what the ISMs trust is the quote, never the address it came from.
pub fn enclave_url(app_id: &str, gateway: &str, chain: &str) -> Result<String> {
    Ok(format!("https://{app_id}-{}.{gateway}", enclave_port(chain)?))
}

/// The registry the enclave images are pushed to.
pub const IMAGE_REPO: &str = "ghcr.io/jonas089/tee-node";

/// The mocha light node Eden's headers are read through.
pub const MOCHA_LIGHT_IMAGE: &str = "ghcr.io/celestiaorg/celestia-node:v0.34.2-mocha";
