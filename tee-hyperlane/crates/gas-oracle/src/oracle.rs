//! Keeping the Celestia IGP's destination gas configs current.
//!
//! Hyperlane quotes a transfer's fee on the origin chain, in the origin's token, for gas that
//! will be spent on the destination chain in the destination's token. Two numbers have to be
//! kept current for that to stay honest as prices move:
//!
//!   gas price       what a unit of destination gas costs, in destination wei
//!   exchange rate   what the destination's token is worth in the origin's token
//!
//! The quote the module computes is
//!
//!   fee = gas_amount * gas_price * exchange_rate / EXCHANGE_RATE_SCALE
//!
//! and `fee` has to come out in the origin's smallest unit. Since the two chains have
//! different decimals, that conversion is folded into the exchange rate rather than left
//! implicit - getting it wrong by 10^12 is the obvious way to under- or over-charge by a
//! factor no one notices until the relayer runs dry.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Hyperlane's fixed-point scale for `token_exchange_rate`.
pub const EXCHANGE_RATE_SCALE: f64 = 1e10;

/// Celestia's `utia`, the token every Celestia-origin quote is denominated in.
pub const LOCAL_DECIMALS: u32 = 6;

/// Effective decimals of the unit `celestia_gas_price` is expressed in: `utia`'s six, plus
/// the six it is scaled by to stay an integer.
pub const CELESTIA_GAS_PRICE_DECIMALS: u32 = LOCAL_DECIMALS + 6;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// How often to push new values. Once an hour is enough: gas prices move, but the quote
    /// only has to be right enough that the relayer is not paid too little.
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    pub igp_id: String,
    pub celestia_rpc: String,
    pub chain_id: String,
    /// Coin id of the origin's own token, for the price feed.
    pub local_price_id: String,
    pub destinations: Vec<Destination>,
    /// EVM chains whose own gas oracles this service also keeps current. Optional: without
    /// them, only Celestia-origin quotes are maintained.
    #[serde(default)]
    pub evm_origins: Vec<EvmOrigin>,
    /// Signs the EVM oracle updates. Read from a file so it is never in the config.
    #[serde(default)]
    pub evm_key_file: Option<String>,
    /// Celestia's gas price, in units of 10^-6 utia per gas.
    ///
    /// Mocha's minimum is 0.004 utia per gas, which is not an integer and Hyperlane's field
    /// is. Scaling by 10^6 keeps it exact; `CELESTIA_GAS_PRICE_DECIMALS` is the matching
    /// correction so the quote still lands on the real cost.
    #[serde(default = "default_celestia_gas_price")]
    pub celestia_gas_price: u128,
    /// Gas a Celestia delivery costs, for the EVM side's quote.
    #[serde(default = "default_celestia_gas_overhead")]
    pub celestia_gas_overhead: u64,
}

fn default_celestia_gas_price() -> u128 {
    // mocha-5's minimum, 0.004 utia per gas, in units of 10^-6 utia.
    4_000
}

fn default_celestia_gas_overhead() -> u64 {
    800_000
}

fn default_interval() -> u64 {
    3600
}

/// One EVM chain's `StorageGasOracle`, quoting Celestia gas for transfers leaving that chain.
///
/// The mirror image of a `Destination`: there the origin is Celestia and the fee is paid in
/// TIA; here the origin is the EVM chain and the fee is paid in its native token.
#[derive(Debug, Clone, Deserialize)]
pub struct EvmOrigin {
    pub name: String,
    pub rpc: String,
    /// The `StorageGasOracle` this service owns and writes.
    pub storage_gas_oracle: String,
    /// Domain of the chain being quoted for - Celestia.
    pub remote_domain: u32,
    /// Decimals of this chain's own native token.
    pub decimals: u32,
    /// Coin id of this chain's native token.
    pub price_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Destination {
    pub name: String,
    pub domain: u32,
    pub rpc: String,
    /// Decimals of the destination's native token; 18 on every EVM chain here.
    pub decimals: u32,
    /// Coin id of the destination's native token, for the price feed.
    pub price_id: String,
    /// Gas the destination spends delivering one message, on top of the message's own cost.
    pub gas_overhead: u64,
}

/// What the oracle last pushed for one destination, and what it was derived from.
#[derive(Debug, Clone, Serialize)]
pub struct Reading {
    pub name: String,
    pub domain: u32,
    pub gas_price_wei: u128,
    pub local_price_usd: f64,
    pub remote_price_usd: f64,
    pub token_exchange_rate: u128,
    pub gas_overhead: u64,
    /// Unix seconds. Absent while the first round is still running.
    pub updated_at: Option<u64>,
    pub error: Option<String>,
}

/// `remote token / local token`, scaled by Hyperlane's 1e10 and corrected for the two chains
/// having different decimals.
///
/// A quote multiplies a destination gas price (in 10^-remote_decimals units) by this, so the
/// decimal correction has to bring the product back into 10^-local_decimals units.
pub fn get_token_exchange_rate(
    local_price_usd: f64,
    remote_price_usd: f64,
    remote_decimals: u32,
) -> u128 {
    get_token_exchange_rate_with_decimals(
        local_price_usd,
        remote_price_usd,
        LOCAL_DECIMALS,
        remote_decimals,
    )
}

/// The same conversion where the origin is not Celestia, so its decimals differ too.
pub fn get_token_exchange_rate_with_decimals(
    local_price_usd: f64,
    remote_price_usd: f64,
    local_decimals: u32,
    remote_decimals: u32,
) -> u128 {
    if local_price_usd <= 0.0 || remote_price_usd <= 0.0 {
        return 0;
    }
    let price_ratio = remote_price_usd / local_price_usd;
    let decimal_correction = 10f64.powi(local_decimals as i32 - remote_decimals as i32);
    (price_ratio * decimal_correction * EXCHANGE_RATE_SCALE) as u128
}

/// Current USD prices for several coin ids, in one request.
///
/// One request rather than one per destination: the public price API rate-limits, and four
/// calls a second earns a 429 whose body parses as JSON but carries no price - which looks
/// exactly like a missing coin unless you go looking.
pub async fn get_token_prices(
    http: &reqwest::Client,
    coin_ids: &[String],
) -> Result<std::collections::HashMap<String, f64>> {
    let ids = coin_ids.join(",");
    let url = format!("https://api.coingecko.com/api/v3/simple/price?ids={ids}&vs_currencies=usd");
    let response = http.get(&url).send().await.context("price feed")?;
    anyhow::ensure!(response.status().is_success(), "price feed returned {}", response.status());

    let body: serde_json::Value = response.json().await?;
    let mut prices = std::collections::HashMap::new();
    for id in coin_ids {
        if let Some(price) = body[id]["usd"].as_f64() {
            prices.insert(id.clone(), price);
        }
    }
    anyhow::ensure!(!prices.is_empty(), "price feed returned no prices for {ids}");
    Ok(prices)
}

/// Current gas price on an EVM chain, in wei.
pub async fn get_gas_price(http: &reqwest::Client, rpc: &str) -> Result<u128> {
    let body: serde_json::Value = http
        .post(rpc)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_gasPrice", "params": []
        }))
        .send()
        .await
        .with_context(|| format!("eth_gasPrice on {rpc}"))?
        .json()
        .await?;
    let hex = body["result"].as_str().context("eth_gasPrice returned no result")?;
    u128::from_str_radix(hex.trim_start_matches("0x"), 16).context("gas price is not hex")
}

/// Push one destination's config to the IGP, and wait for the chain to accept it.
///
/// Signing goes through `celestia-appd` for the same reason the relayer does: the key pays
/// gas and nothing more, and reimplementing cosmos tx signing to save one process is a poor
/// trade.
///
/// Waiting matters more than it looks. A cosmos tx reports success as soon as it passes
/// CheckTx, and several txs from one account in quick succession collide on sequence - the
/// first is accepted and the rest are silently rejected. Rounds are sequential and confirmed
/// for that reason.
pub fn set_destination_gas_config(config: &Config, reading: &Reading) -> Result<String> {
    let home = std::env::var("CELHOME").unwrap_or_else(|_| "/tmp/celhome".into());
    let output = std::process::Command::new("celestia-appd")
        .args([
            "tx", "hyperlane", "hooks", "igp", "set-destination-gas-config",
            &config.igp_id,
            &reading.domain.to_string(),
            &reading.token_exchange_rate.to_string(),
            &reading.gas_price_wei.to_string(),
            &reading.gas_overhead.to_string(),
            "--from", "bridge",
            "--home", &home,
            "--keyring-backend", "test",
            "--chain-id", &config.chain_id,
            "--node", &config.celestia_rpc,
            "--fees", "5000utia",
            "--gas", "300000",
            "-y", "-o", "json",
        ])
        .output()
        .context("running celestia-appd")?;

    anyhow::ensure!(
        output.status.success(),
        "celestia-appd failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let response: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("celestia-appd did not return json")?;
    let code = response["code"].as_u64().unwrap_or_default();
    anyhow::ensure!(code == 0, "rejected: {}", response["raw_log"].as_str().unwrap_or(""));

    let hash = response["txhash"].as_str().context("no txhash")?.to_string();
    wait_for_tx(config, &hash)?;
    Ok(hash)
}

/// Block until a tx is in a block, and fail if it failed there.
fn wait_for_tx(config: &Config, hash: &str) -> Result<()> {
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let output = std::process::Command::new("celestia-appd")
            .args(["query", "tx", hash, "--node", &config.celestia_rpc, "-o", "json"])
            .output()
            .context("celestia-appd query tx")?;
        if !output.status.success() {
            continue;
        }
        let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let code = result["code"].as_u64().unwrap_or_default();
        anyhow::ensure!(code == 0, "failed: {}", result["raw_log"].as_str().unwrap_or(""));
        return Ok(());
    }
    anyhow::bail!("timed out waiting for {hash}")
}

/// Read prices and gas, then push one round of configs.
pub async fn run_once(config: &Config, http: &reqwest::Client) -> Vec<Reading> {
    let mut coin_ids = vec![config.local_price_id.clone()];
    for destination in &config.destinations {
        if !coin_ids.contains(&destination.price_id) {
            coin_ids.push(destination.price_id.clone());
        }
    }
    let prices = get_token_prices(http, &coin_ids).await;
    let mut readings = Vec::new();

    for destination in &config.destinations {
        let mut reading = Reading {
            name: destination.name.clone(),
            domain: destination.domain,
            gas_price_wei: 0,
            local_price_usd: 0.0,
            remote_price_usd: 0.0,
            token_exchange_rate: 0,
            gas_overhead: destination.gas_overhead,
            updated_at: None,
            error: None,
        };

        match gather(config, http, destination, &prices).await {
            Ok(filled) => {
                reading = Reading { name: reading.name, ..filled };
                match set_destination_gas_config(config, &reading) {
                    Ok(_) => reading.updated_at = Some(now()),
                    Err(e) => reading.error = Some(e.to_string()),
                }
            }
            Err(e) => reading.error = Some(e.to_string()),
        }
        readings.push(reading);
    }
    readings.extend(run_evm_origins(config, &prices).await);
    readings
}

async fn gather(
    config: &Config,
    http: &reqwest::Client,
    destination: &Destination,
    prices: &Result<std::collections::HashMap<String, f64>>,
) -> Result<Reading> {
    let prices = prices.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
    let local_price_usd = *prices
        .get(&config.local_price_id)
        .with_context(|| format!("no price for {}", config.local_price_id))?;
    let remote_price_usd = *prices
        .get(&destination.price_id)
        .with_context(|| format!("no price for {}", destination.price_id))?;
    let gas_price_wei = get_gas_price(http, &destination.rpc).await?;

    Ok(Reading {
        name: destination.name.clone(),
        domain: destination.domain,
        gas_price_wei,
        local_price_usd,
        remote_price_usd,
        token_exchange_rate: get_token_exchange_rate(
            local_price_usd,
            remote_price_usd,
            destination.decimals,
        ),
        gas_overhead: destination.gas_overhead,
        updated_at: None,
        error: None,
    })
}

/// Push Celestia's gas price and the TIA exchange rate to one EVM chain's gas oracle.
///
/// Signing goes through `cast` for the same reason the relayer does: this key pays gas and
/// owns a write-only oracle, and nothing else.
pub fn set_evm_gas_data(
    config: &Config,
    origin: &EvmOrigin,
    exchange_rate: u128,
) -> Result<String> {
    let key_file = config
        .evm_key_file
        .as_ref()
        .context("evm_origins are configured but evm_key_file is not")?;
    let key = std::fs::read_to_string(key_file)
        .with_context(|| format!("reading {key_file}"))?
        .trim()
        .to_string();
    let key = if key.starts_with("0x") { key } else { format!("0x{key}") };

    let configs = format!(
        "[({},{},{})]",
        origin.remote_domain, exchange_rate, config.celestia_gas_price
    );
    let output = std::process::Command::new("cast")
        .args([
            "send",
            &origin.storage_gas_oracle,
            "setRemoteGasDataConfigs((uint32,uint128,uint128)[])",
            &configs,
            "--rpc-url",
            &origin.rpc,
            "--private-key",
            &key,
            "--json",
        ])
        .output()
        .context("running cast")?;

    anyhow::ensure!(
        output.status.success(),
        "cast failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let status = receipt["status"].as_str().unwrap_or("0x0");
    anyhow::ensure!(status == "0x1", "oracle update reverted");
    Ok(receipt["transactionHash"].as_str().unwrap_or_default().to_string())
}

/// Update every configured EVM chain's own gas oracle.
pub async fn run_evm_origins(
    config: &Config,
    prices: &Result<std::collections::HashMap<String, f64>>,
) -> Vec<Reading> {
    let mut readings = Vec::new();
    for origin in &config.evm_origins {
        let mut reading = Reading {
            name: format!("{} to Celestia", origin.name),
            domain: origin.remote_domain,
            gas_price_wei: config.celestia_gas_price,
            local_price_usd: 0.0,
            remote_price_usd: 0.0,
            token_exchange_rate: 0,
            gas_overhead: config.celestia_gas_overhead,
            updated_at: None,
            error: None,
        };

        match prices.as_ref() {
            Err(e) => reading.error = Some(e.to_string()),
            Ok(prices) => {
                // Here the local token is the EVM chain's, and the remote one is TIA.
                let local = prices.get(&origin.price_id).copied().unwrap_or_default();
                let remote = prices.get(&config.local_price_id).copied().unwrap_or_default();
                reading.local_price_usd = local;
                reading.remote_price_usd = remote;
                // Local is this EVM chain, remote is Celestia - the mirror of the other
                // direction, so the decimals swap too.
                reading.token_exchange_rate = get_token_exchange_rate_with_decimals(
                    local,
                    remote,
                    origin.decimals,
                    CELESTIA_GAS_PRICE_DECIMALS,
                );

                match set_evm_gas_data(config, origin, reading.token_exchange_rate) {
                    Ok(_) => reading.updated_at = Some(now()),
                    Err(e) => reading.error = Some(e.to_string()),
                }
            }
        }
        readings.push(reading);
    }
    readings
}

/// What a chain currently says its gas config is, read back rather than remembered.
///
/// The page shows this next to what was last pushed, so a round that silently failed - or a
/// value changed by someone else - is visible instead of implied.
#[derive(Debug, Clone, Serialize)]
pub struct OnChainConfig {
    pub chain: String,
    pub remote_domain: u32,
    pub token_exchange_rate: String,
    pub gas_price: String,
    pub gas_overhead: Option<String>,
    pub error: Option<String>,
}

/// Every destination gas config the Celestia IGP holds.
pub fn read_celestia_configs(config: &Config) -> Vec<OnChainConfig> {
    let output = std::process::Command::new("celestia-appd")
        .args([
            "query", "hyperlane", "hooks", "destination-gas-configs",
            &config.igp_id, "--node", &config.celestia_rpc, "-o", "json",
        ])
        .output();

    let fallback = |error: String| {
        vec![OnChainConfig {
            chain: "Celestia mocha-5".into(),
            remote_domain: 0,
            token_exchange_rate: "—".into(),
            gas_price: "—".into(),
            gas_overhead: None,
            error: Some(error),
        }]
    };

    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(output) => return fallback(String::from_utf8_lossy(&output.stderr).trim().into()),
        Err(e) => return fallback(e.to_string()),
    };

    let parsed: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(parsed) => parsed,
        Err(e) => return fallback(e.to_string()),
    };

    parsed["destination_gas_configs"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|entry| OnChainConfig {
                    chain: "Celestia mocha-5".into(),
                    remote_domain: entry["remote_domain"].as_u64().unwrap_or_default() as u32,
                    token_exchange_rate: text(&entry["gas_oracle"]["token_exchange_rate"]),
                    gas_price: text(&entry["gas_oracle"]["gas_price"]),
                    gas_overhead: Some(text(&entry["gas_overhead"])),
                    error: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What each EVM chain's own oracle currently quotes for Celestia.
pub fn read_evm_configs(config: &Config) -> Vec<OnChainConfig> {
    config
        .evm_origins
        .iter()
        .map(|origin| {
            let mut entry = OnChainConfig {
                chain: origin.name.clone(),
                remote_domain: origin.remote_domain,
                token_exchange_rate: "—".into(),
                gas_price: "—".into(),
                gas_overhead: None,
                error: None,
            };
            match read_evm_gas_data(origin) {
                Ok((rate, price)) => {
                    entry.token_exchange_rate = rate;
                    entry.gas_price = price;
                }
                Err(e) => entry.error = Some(e.to_string()),
            }
            entry
        })
        .collect()
}

fn read_evm_gas_data(origin: &EvmOrigin) -> Result<(String, String)> {
    let output = std::process::Command::new("cast")
        .args([
            "call",
            &origin.storage_gas_oracle,
            "getExchangeRateAndGasPrice(uint32)(uint128,uint128)",
            &origin.remote_domain.to_string(),
            "--rpc-url",
            &origin.rpc,
        ])
        .output()
        .context("running cast")?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let text = String::from_utf8(output.stdout)?;
    let mut lines = text.lines().map(|line| {
        line.split_whitespace().next().unwrap_or_default().to_string()
    });
    let rate = lines.next().context("no exchange rate returned")?;
    let price = lines.next().context("no gas price returned")?;
    Ok((rate, price))
}

fn text(value: &serde_json::Value) -> String {
    value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string())
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decimal correction is the part that is easy to get wrong and expensive to get
    /// wrong: TIA has 6 decimals and ETH has 18, so a quote that skipped it would be off by
    /// a factor of 10^12.
    #[test]
    fn the_exchange_rate_folds_in_the_decimal_difference() {
        // 1 TIA = $5, 1 ETH = $2500, so one ETH is worth 500 TIA.
        let rate = get_token_exchange_rate(5.0, 2500.0, 18);

        // 500 * 10^(6-18) * 10^10 = 500 * 10^-2 = 5
        assert_eq!(rate, 5);

        // A 21000-gas delivery at 1 gwei costs 21000 * 1e9 wei = 2.1e13 wei.
        // fee = 2.1e13 * 5 / 1e10 = 10500 utia = 0.0105 TIA.
        let fee = 21_000u128 * 1_000_000_000 * rate / EXCHANGE_RATE_SCALE as u128;
        assert_eq!(fee, 10_500);

        // Cross-check against the same trade priced in USD directly.
        let eth_spent = 21_000.0 * 1e9 / 1e18;
        let usd = eth_spent * 2500.0;
        let tia = fee as f64 / 1e6;
        assert!((tia * 5.0 - usd).abs() < 1e-9, "{tia} TIA should be {usd} USD");
    }

    /// The reverse direction: an EVM chain quoting Celestia gas. Both the decimals and the
    /// price ratio invert, and the gas price is carried scaled, so this is the arrangement
    /// most likely to be wrong by orders of magnitude.
    #[test]
    fn an_evm_origin_quotes_celestia_delivery_at_its_real_cost() {
        // 1 ETH = $2500, 1 TIA = $5.
        let rate = get_token_exchange_rate_with_decimals(2500.0, 5.0, 18, CELESTIA_GAS_PRICE_DECIMALS);

        // 800k gas at 0.004 utia = 3200 utia = 0.0032 TIA = $0.016.
        let gas_price = 4_000u128; // 0.004 utia, scaled by 10^6
        let fee_wei = 800_000u128 * gas_price * rate / EXCHANGE_RATE_SCALE as u128;
        let usd = (fee_wei as f64 / 1e18) * 2500.0;
        assert!((usd - 0.016).abs() < 1e-4, "quoted ${usd}, should be about $0.016");
    }

    #[test]
    fn a_missing_local_price_does_not_produce_a_free_transfer() {
        assert_eq!(get_token_exchange_rate(0.0, 2500.0, 18), 0);
    }
}
