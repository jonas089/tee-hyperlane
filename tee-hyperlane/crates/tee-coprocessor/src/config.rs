//! Loads the coprocessor config: the chains, the routes between them, and their settings.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::destination::{self, Destination};
use crate::origin::l1::{celestia, ethereum};
use crate::origin::l2::{arbitrum, base, eden};
use crate::origin::{Cache, Indexer};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// How often each route looks for work.
    #[serde(default = "default_tick")]
    pub tick_secs: u64,
    /// Where routes keep their staged and finished batches, and chains their caches.
    pub proof_dir: String,
    /// Where the dashboard API listens.
    #[serde(default = "default_api")]
    pub api_listen: String,
    pub chains: BTreeMap<String, toml::Table>,
    #[serde(default)]
    pub routes: Vec<Route>,
    /// Test TIA for anyone who asks, from a funded key on a Celestia chain. Off when absent.
    pub faucet: Option<crate::api::FaucetConfig>,
    /// Swaps across chains through the pools and the bridge, under `/api/v1/trade`. Off when
    /// absent.
    pub trade: Option<crate::api::trade::TradeConfig>,
    /// How often the tracker reads each origin for new transfers and checks each one's
    /// progress.
    #[serde(default = "default_track")]
    pub track_secs: u64,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub name: String,
    pub from: String,
    pub to: String,
    /// The enclave that attests `from`.
    pub enclave: String,
    /// The ISM on `to` that this route advances.
    pub ism: String,
    /// Our warp routers on `to`, as 32-byte Hyperlane addresses. Only a batch carrying a message
    /// for one of them starts an attestation; with none listed, any message for `to` does. This
    /// decides when a route works, never what it delivers: a batch always carries every leaf.
    #[serde(default)]
    pub routers: Vec<String>,
    /// How long a transfer on this route may take before it is reported overdue. Defaults by
    /// origin: see `tracker::expected_latency`.
    pub expected_latency_secs: Option<u64>,
    /// Attest once the origin head is this many blocks past the ISM's trusted height, whether
    /// or not anything is for us. Keeps the proof at the trusted height recent, which a full
    /// node needs: it rebuilds old state from the head, slower the further back it goes.
    pub max_lag: Option<u64>,
}

fn default_tick() -> u64 {
    60
}
/// Chain fields only the tracker and the explorer read, removed before a chain's own `Config`
/// sees its table: `explorer` for links, and the gas wallet's token and warning level.
pub const REPORTING_KEYS: &[&str] = &["explorer", "gas_symbol", "gas_decimals", "low_balance"];

fn default_track() -> u64 {
    15
}
fn default_api() -> String {
    "0.0.0.0:3001".into()
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let config: Self =
            toml::from_str(&std::fs::read_to_string(path).with_context(|| path.to_string())?)?;
        for route in &config.routes {
            config.chain::<toml::Table>(&route.from)?;
            config.chain::<toml::Table>(&route.to)?;
        }
        Ok(config)
    }

    pub fn proof_dir(&self) -> PathBuf {
        match (self.proof_dir.strip_prefix("~/"), std::env::var("HOME")) {
            (Some(rest), Ok(home)) => PathBuf::from(home).join(rest),
            _ => PathBuf::from(&self.proof_dir),
        }
    }

    pub fn kind(&self, name: &str) -> Result<&str> {
        self.chains
            .get(name)
            .with_context(|| format!("no chain `{name}` in the config"))?
            .get("kind")
            .and_then(|k| k.as_str())
            .with_context(|| format!("chain `{name}` has no `kind`"))
    }

    /// A chain's table as its kind's `Config`.
    pub fn chain<T: DeserializeOwned>(&self, name: &str) -> Result<T> {
        let mut table = self
            .chains
            .get(name)
            .with_context(|| format!("no chain `{name}` in the config"))?
            .clone();
        // Read by the tracker and the explorer, never by the chain's own `Config`.
        table.remove("kind");
        for key in crate::config::REPORTING_KEYS {
            table.remove(*key);
        }
        table.try_into().with_context(|| format!("chain `{name}`"))
    }

    pub fn domain(&self, name: &str) -> Result<u32> {
        let table = self
            .chains
            .get(name)
            .with_context(|| format!("no chain `{name}` in the config"))?;
        let domain = table
            .get("domain")
            .and_then(|d| d.as_integer())
            .with_context(|| format!("chain `{name}` has no `domain`"))?;
        Ok(domain as u32)
    }

    /// The indexer for `name` as an origin.
    pub fn indexer(&self, name: &str) -> Result<Box<dyn Indexer>> {
        let cache = Cache::new(self.proof_dir().join("chains").join(name));
        Ok(match self.kind(name)? {
            "ethereum" => Box::new(ethereum::Ethereum::new(self.chain(name)?, cache)?),
            "arbitrum" => Box::new(arbitrum::Arbitrum::new(self.chain(name)?)?),
            "base" => Box::new(base::Base::new(self.chain(name)?)?),
            "celestia" => Box::new(celestia::Celestia::new(self.chain(name)?)?),
            "eden" => {
                let config: eden::Config = self.chain(name)?;
                let parent = self.chain(&config.celestia)?;
                Box::new(eden::Eden::new(config, parent, cache)?)
            }
            other => anyhow::bail!("chain `{name}` has unknown kind `{other}`"),
        })
    }

    /// The destination for `name`, delivering through `ism`.
    pub fn destination(&self, name: &str, ism: &str) -> Result<Box<dyn Destination>> {
        #[derive(Deserialize)]
        struct Endpoint {
            domain: u32,
            rpc: String,
            send_rpc: Option<String>,
            mailbox: Option<String>,
        }
        let table = self
            .chains
            .get(name)
            .with_context(|| format!("no chain `{name}` in the config"))?;
        let mut table = table.clone();
        for key in crate::config::REPORTING_KEYS {
            table.remove(*key);
        }
        let endpoint: Endpoint = table
            .try_into()
            .with_context(|| format!("chain `{name}`"))?;
        let mailbox = endpoint
            .mailbox
            .with_context(|| format!("chain `{name}` needs `mailbox` as a destination"))?;
        Ok(match self.kind(name)? {
            "celestia" => {
                let c: celestia::Config = self.chain(name)?;
                Box::new(destination::Celestia {
                    rpc: endpoint.rpc,
                    mailbox,
                    domain: endpoint.domain,
                    ism: ism.to_string(),
                    chain_id: c.chain_id,
                    key: c.key,
                    home: c
                        .home
                        .or_else(|| std::env::var("CELHOME").ok())
                        .context("a Celestia destination needs `home` or CELHOME")?,
                })
            }
            _ => Box::new(destination::Evm {
                rpc: endpoint.send_rpc.unwrap_or(endpoint.rpc),
                mailbox,
                domain: endpoint.domain,
                ism: ism.to_string(),
            }),
        })
    }
}
