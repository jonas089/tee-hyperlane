//! The bridge's Celestia chain, through the celestia-appd binary built from the
//! `jonas/tee-ism` branch (`teeism build`).
//!
//! A transaction counts as done only once it is in a block with code 0; anything else stops
//! the step with the node's own log.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::shared::chains::{self, celestia};
use crate::shared::settings::Ctx;
use crate::shared::shell::{first_json, Cmd};

pub struct Appd {
    bin: PathBuf,
    home: PathBuf,
    rpc: String,
}

pub struct Tx {
    pub hash: String,
    pub json: Value,
}

impl Tx {
    /// One attribute of the first event whose type matches `event`.
    pub fn attr(&self, event: &str, key: &str) -> Result<String> {
        self.find_attr(|t| t == event, key)
            .with_context(|| format!("tx {} emitted no {event} with a {key}", self.hash))
    }

    /// As `attr`, matching the event type by a predicate.
    pub fn find_attr(&self, matches: impl Fn(&str) -> bool, key: &str) -> Option<String> {
        self.json["events"].as_array()?.iter().find_map(|ev| {
            if !matches(ev["type"].as_str()?) {
                return None;
            }
            ev["attributes"].as_array()?.iter().find_map(|a| {
                (a["key"].as_str()? == key)
                    .then(|| a["value"].as_str().unwrap_or("").trim_matches('"').to_string())
            })
        })
    }
}

impl Appd {
    pub fn new(ctx: &Ctx) -> Result<Self> {
        let bin = ctx.appd();
        if !bin.is_file() {
            bail!("no celestia-appd at {}; run `teeism build`", bin.display());
        }
        Ok(Self {
            bin,
            home: ctx.celestia_home(),
            rpc: chains::celestia_rpc(ctx),
        })
    }

    fn cmd(&self, what: &str) -> Cmd {
        Cmd::new(what, &self.bin)
    }

    /// `celestia-appd query <args>` as JSON.
    pub fn query(&self, what: &str, args: &[&str]) -> Result<Value> {
        let out = self
            .cmd(what)
            .arg("query")
            .args(args)
            .args(["--node", &self.rpc, "--output", "json"])
            .output()?;
        serde_json::from_str(&out).with_context(|| format!("{what}: the node's answer is not JSON: {out}"))
    }

    /// Broadcast a transaction signed by a keyring key, wait for it to land, and return it.
    pub fn tx(&self, what: &str, from: &str, args: &[&str]) -> Result<Tx> {
        self.tx_with_gas(what, from, args, celestia::GAS)
    }

    pub fn tx_with_gas(&self, what: &str, from: &str, args: &[&str], gas: &str) -> Result<Tx> {
        let home = self.home.to_string_lossy().into_owned();
        let (status, stdout, stderr) = self
            .cmd(what)
            .arg("tx")
            .args(args)
            .args([
                "--from",
                from,
                "--keyring-backend",
                "test",
                "--home",
                &home,
                "--chain-id",
                celestia::CHAIN_ID,
                "--node",
                &self.rpc,
                "--gas-prices",
                celestia::GAS_PRICES,
                "--gas",
                gas,
                "--broadcast-mode",
                "sync",
                "--yes",
                "--output",
                "json",
            ])
            .capture()?;
        let raw = format!("{stdout}\n{stderr}");
        if !status.success() {
            bail!(
                "{what}: could not be broadcast:\n{}",
                crate::shared::shell::tail(&raw, 20)
            );
        }
        let sent =
            first_json(&stdout).with_context(|| format!("{what}: the node returned no JSON:\n{raw}"))?;
        // CheckTx: a non-zero code here means it never entered the mempool.
        if sent["code"].as_u64().unwrap_or(0) != 0 {
            bail!(
                "{what}: rejected with code {}: {}",
                sent["code"],
                sent["raw_log"].as_str().unwrap_or("")
            );
        }
        let hash = sent["txhash"]
            .as_str()
            .with_context(|| format!("{what}: the node returned no tx hash: {sent}"))?
            .to_string();
        self.wait_for_tx(what, &hash)
    }

    pub fn wait_for_tx(&self, what: &str, hash: &str) -> Result<Tx> {
        for _ in 0..60 {
            let (status, stdout, _) = self
                .cmd(what)
                .args(["query", "tx", hash, "--node", &self.rpc, "--output", "json"])
                .capture()?;
            if status.success() {
                let json: Value = serde_json::from_str(&stdout)
                    .with_context(|| format!("{what}: tx {hash} is not JSON: {stdout}"))?;
                let code = json["code"].as_u64().unwrap_or(0);
                if code != 0 {
                    bail!(
                        "{what}: tx {hash} failed on chain with code {code}: {}",
                        json["raw_log"].as_str().unwrap_or("no log")
                    );
                }
                return Ok(Tx {
                    hash: hash.to_string(),
                    json,
                });
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        bail!("{what}: tx {hash} was not included within 60s; check `celestia-appd query tx {hash}`")
    }

    /// The bech32 address of a keyring key.
    pub fn address(&self, key: &str) -> Result<String> {
        let home = self.home.to_string_lossy().into_owned();
        let out = self
            .cmd(&format!("reading the '{key}' key"))
            .args([
                "keys",
                "show",
                key,
                "-a",
                "--keyring-backend",
                "test",
                "--home",
                &home,
            ])
            .output()
            .with_context(|| {
                format!("no '{key}' key in the keyring at {home}; `teeism celestia up` creates it")
            })?;
        Ok(out.trim().to_string())
    }

    pub fn has_key(&self, key: &str) -> Result<bool> {
        let home = self.home.to_string_lossy().into_owned();
        let (status, _, _) = self
            .cmd("looking up a key")
            .args(["keys", "show", key, "--keyring-backend", "test", "--home", &home])
            .capture()?;
        Ok(status.success())
    }

    /// Add a key derived from `mnemonic` at `account` to the keyring.
    pub fn recover_key(&self, key: &str, account: u32, mnemonic: &str) -> Result<()> {
        let home = self.home.to_string_lossy().into_owned();
        self.cmd(&format!("recovering the '{key}' key at account {account}"))
            .args([
                "keys",
                "add",
                key,
                "--recover",
                "--account",
                &account.to_string(),
                "--keyring-backend",
                "test",
                "--home",
                &home,
                "--output",
                "json",
            ])
            .stdin(format!("{mnemonic}\n"))
            .output()?;
        Ok(())
    }

    pub fn utia_balance(&self, address: &str) -> Result<u128> {
        self.balance(address, celestia::DENOM)
    }

    pub fn balance(&self, address: &str, denom: &str) -> Result<u128> {
        let v = self.query(
            &format!("reading {address}'s {denom}"),
            &["bank", "balance", address, denom],
        )?;
        let amount = v["balance"]["amount"].as_str().unwrap_or("0");
        amount
            .parse()
            .with_context(|| format!("balance {amount} of {address} is not a number"))
    }

    pub fn total_supply(&self, denom: &str) -> Result<u128> {
        let v = self.query(
            &format!("reading the {denom} supply"),
            &["bank", "total-supply-of", denom],
        )?;
        let amount = v["amount"]["amount"]
            .as_str()
            .or_else(|| v["amount"].as_str())
            .unwrap_or("0");
        amount
            .parse()
            .with_context(|| format!("supply {amount} of {denom} is not a number"))
    }

    pub fn bin(&self) -> &PathBuf {
        &self.bin
    }
}

/// The chain's height, or an error saying it is not answering.
pub async fn height(ctx: &Ctx) -> Result<u64> {
    let rpc = chains::celestia_rpc(ctx);
    let v: Value = reqwest::Client::new()
        .get(format!("{rpc}/status"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .with_context(|| format!("celestia at {rpc} is not answering"))?
        .json()
        .await
        .with_context(|| format!("celestia at {rpc} returned something other than JSON from /status"))?;
    v["result"]["sync_info"]["latest_block_height"]
        .as_str()
        .and_then(|h| h.parse().ok())
        .with_context(|| format!("celestia at {rpc} reported no height: {v}"))
}

/// Block until the chain answers and has produced a block.
pub async fn wait_for_chain(ctx: &Ctx) -> Result<u64> {
    let mut last = None;
    for _ in 0..90 {
        match height(ctx).await {
            Ok(h) if h > 0 => return Ok(h),
            Ok(_) => {}
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let why = last.map(|e| format!(": {e:#}")).unwrap_or_default();
    bail!("celestia did not start producing blocks within 90s{why}; check `docker logs teeism-celestia`")
}

/// The 20 bytes behind a bech32 address, as a 32-byte Hyperlane address.
pub fn bech32_to_hex32(addr: &str) -> Result<String> {
    let (_, data) = bech32::decode(addr).with_context(|| format!("{addr} is not a bech32 address"))?;
    if data.len() != 20 {
        bail!("{addr} decodes to {} bytes, not 20", data.len());
    }
    Ok(format!("0x{}{}", "0".repeat(24), hex::encode(data)))
}

/// A 20-byte EVM address as the 32 bytes Hyperlane carries: lowercase, left-padded.
pub fn pad32(addr: &alloy::primitives::Address) -> String {
    format!("0x{}{}", "0".repeat(24), hex::encode(addr.as_slice()))
}

/// The routers a warp token has enrolled: domain -> 32-byte receiver.
pub fn remote_routers(appd: &Appd, token: &str) -> Result<Vec<(u32, String)>> {
    let v = appd.query(
        &format!("reading the remote routers of {token}"),
        &["warp", "remote-routers", token],
    )?;
    let mut out = Vec::new();
    for r in v["remote_routers"].as_array().cloned().unwrap_or_default() {
        let domain = r["receiver_domain"]
            .as_u64()
            .or_else(|| r["receiver_domain"].as_str().and_then(|s| s.parse().ok()))
            .with_context(|| format!("a remote router of {token} has no domain: {r}"))?;
        let contract = r["receiver_contract"].as_str().unwrap_or("").to_lowercase();
        out.push((domain as u32, contract));
    }
    Ok(out)
}
