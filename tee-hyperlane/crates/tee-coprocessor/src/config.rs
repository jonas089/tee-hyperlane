//! Loads the coprocessor config: the chains, the routes between them, and their settings.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::destination::{self, Destination};
#[cfg(feature = "celestia")]
use crate::origin::l1::celestia;
#[cfg(feature = "ethereum")]
use crate::origin::l1::ethereum;
#[cfg(feature = "arbitrum")]
use crate::origin::l2::arbitrum;
#[cfg(feature = "base")]
use crate::origin::l2::base;
#[cfg(feature = "eden")]
use crate::origin::l2::eden;
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
    /// How often the tracker reads each origin for new transfers and checks each one's
    /// progress.
    #[serde(default = "default_track")]
    pub track_secs: u64,
    /// What `teeism upstream check` found, served to the dashboard. Off when absent.
    #[serde(default)]
    pub upstream_file: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub name: String,
    pub from: String,
    pub to: String,
    /// The enclaves that may attest `from`, each one the ISM pins. Asked in turn, starting with
    /// the last that answered, so a second enclave keeps the route alive while the first is
    /// down or being rotated out. A config written for one enclave, `enclave = "..."`, still
    /// reads as a list of one.
    #[serde(alias = "enclave", deserialize_with = "one_or_many")]
    pub enclaves: Vec<String>,
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

/// `enclaves = [...]`, or the one-enclave form `enclave = "..."` from before ISMs pinned sets.
fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Many(Vec<String>),
        One(String),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::Many(list) => list,
        OneOrMany::One(url) => vec![url],
    })
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

    /// Where chain `name` keeps what it fetched.
    pub fn cache(&self, name: &str) -> Cache {
        Cache::new(self.proof_dir().join("chains").join(name))
    }

    /// The indexer for `name` as an origin.
    pub fn indexer(&self, name: &str) -> Result<Box<dyn Indexer>> {
        Ok(match self.kind(name)? {
            #[cfg(feature = "ethereum")]
            "ethereum" => Box::new(ethereum::Ethereum::new(
                self.chain(name)?,
                self.cache(name),
            )?),
            #[cfg(feature = "arbitrum")]
            "arbitrum" => Box::new(arbitrum::Arbitrum::new(self.chain(name)?)?),
            #[cfg(feature = "base")]
            "base" => Box::new(base::Base::new(self.chain(name)?)?),
            #[cfg(feature = "celestia")]
            "celestia" => Box::new(celestia::Celestia::new(self.chain(name)?)?),
            #[cfg(feature = "eden")]
            "eden" => {
                let config: eden::Config = self.chain(name)?;
                let parent = self.chain(&config.celestia)?;
                Box::new(eden::Eden::new(config, parent, self.cache(name))?)
            }
            other => return Err(unsupported(name, other)),
        })
    }

    /// The destination for `name`, delivering through `ism`.
    pub fn destination(&self, name: &str, ism: &str) -> Result<Box<dyn Destination>> {
        #[derive(Deserialize)]
        struct Endpoint {
            domain: u32,
            rpc: String,
            #[cfg(any(
                feature = "ethereum",
                feature = "base",
                feature = "arbitrum",
                feature = "eden"
            ))]
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
            #[cfg(feature = "celestia")]
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
                    gas_price: c.gas_price,
                })
            }
            #[cfg(any(
                feature = "ethereum",
                feature = "base",
                feature = "arbitrum",
                feature = "eden"
            ))]
            "ethereum" | "base" | "arbitrum" | "eden" => Box::new(destination::Evm {
                rpc: endpoint.send_rpc.unwrap_or(endpoint.rpc),
                mailbox,
                domain: endpoint.domain,
                ism: ism.to_string(),
            }),
            other => return Err(unsupported(name, other)),
        })
    }
}

/// Every chain kind the relayer knows, whether or not this build includes it.
pub const KINDS: &[&str] = &["celestia", "ethereum", "base", "arbitrum", "eden"];

/// Why chain `name` of kind `kind` cannot be served: the kind is not one the relayer knows, or
/// this build was made without it.
pub fn unsupported(name: &str, kind: &str) -> anyhow::Error {
    if KINDS.contains(&kind) {
        anyhow::anyhow!(
            "chain `{name}` has kind `{kind}`, which this build does not include; build tee-coprocessor with the `{kind}` feature"
        )
    } else {
        anyhow::anyhow!("chain `{name}` has unknown kind `{kind}`")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A route names its enclaves as a list, or one enclave by the old key.
    #[test]
    fn a_route_reads_one_enclave_or_a_list() {
        let one: Route = toml::from_str(
            r#"name = "a"
from = "celestia"
to = "eden"
enclave = "https://x-8080.g"
ism = "0x1""#,
        )
        .unwrap();
        assert_eq!(one.enclaves, vec!["https://x-8080.g"]);
        let many: Route = toml::from_str(
            r#"name = "a"
from = "celestia"
to = "eden"
enclaves = ["https://x-8080.g", "https://y-8080.g"]
ism = "0x1""#,
        )
        .unwrap();
        assert_eq!(many.enclaves.len(), 2);
    }
}
