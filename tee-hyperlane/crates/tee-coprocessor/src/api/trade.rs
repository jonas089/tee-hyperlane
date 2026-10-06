//! `/api/v1/trade`: a cross-chain venue over Uniswap v3 pools on the EVM chains, bridged
//! through the Celestia hub, with tokens anyone can launch.
//!
//! A trade is a list of steps. A swap runs on one chain with pools; a bridge moves one asset
//! between the hub and an EVM chain, 1:1, so EVM to EVM is two bridges. A swap takes the direct
//! pool for a pair or the route through the quote asset (teeUSD), whichever pays more.
//!
//! A launched token lives on the hub as a synthetic with a fixed supply, and on each EVM chain
//! as a router made by that chain's token factory. It is listed once the hub shows it wired the
//! way the venue requires: ownership renounced, the routing ISM, and every remote router one
//! the factories made. Its routers are then added to `crate::registry`, so the relayer carries
//! its transfers like any of ours.
//!
//! Nothing here signs. Every endpoint returns unsigned transactions for the caller's wallet.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{keccak256, U256, U512};
use anyhow::{bail, Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::origin::evm::Rpc;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TradeConfig {
    /// The Celestia chain every route goes through.
    #[serde(default = "default_hub")]
    pub hub: String,
    /// The hub's REST endpoint, where launched tokens are checked.
    pub hub_rest: String,
    pub hub_mailbox: String,
    /// The ISM a launch mints through once, and the one it must end on.
    pub noop_ism: String,
    pub routing_ism: String,
    /// The asset every pool is quoted in and multi-hop swaps route through.
    #[serde(default = "default_quote")]
    pub quote_asset: String,
    /// Where a route swaps when neither end has the pools it needs.
    pub default_venue: String,
    /// What a hub-origin transfer offers the paymaster at most, in utia. The module charges
    /// only the live quote, so this bounds the fee rather than setting it.
    #[serde(default = "default_max_fee")]
    pub celestia_max_fee: u64,
    /// Destination gas a launched token enrolls its routers with, as ours are.
    #[serde(default = "default_router_gas")]
    pub router_gas: u64,
    /// The assets this deployment issues: TIA and teeUSD.
    pub assets: BTreeMap<String, Asset>,
    pub venues: BTreeMap<String, Venue>,
}

fn default_hub() -> String {
    "celestia".into()
}
fn default_quote() -> String {
    "teeUSD".into()
}
fn default_max_fee() -> u64 {
    10_000_000
}
fn default_router_gas() -> u64 {
    50_000
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub decimals: u8,
    /// The bank denom on the hub.
    pub denom: String,
    /// The warp router per chain: a token id on the hub, the ERC20 itself on an EVM chain.
    pub routers: BTreeMap<String, String>,
}

/// Uniswap v3 on one chain, and the token factory there.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all(serialize = "camelCase"))]
pub struct Venue {
    pub factory: String,
    pub positions: String,
    pub swap_router: String,
    pub quoter: String,
    /// The fee tier every pool here uses, in hundredths of a basis point.
    pub fee: u32,
    pub token_factory: Option<String>,
    /// Where the venue reads this chain, when not the chain's own endpoints: a full node
    /// busy proving old state answers slowly and can lag the head.
    #[serde(default, skip_serializing)]
    pub rpc: Option<String>,
}

/// Where a launch was made, before the hub has shown it wired.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub chain: String,
    pub router: String,
    pub launcher: String,
    pub name: String,
    pub symbol: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetView {
    /// The symbol for the deployment's own assets, the hub token id for a launched one.
    pub id: String,
    pub symbol: String,
    pub name: String,
    pub decimals: u8,
    pub denom: String,
    pub routers: BTreeMap<String, String>,
    pub launched: bool,
    pub launcher: Option<String>,
    /// Pools against the quote asset with liquidity, by chain.
    pub pools: BTreeMap<String, String>,
}

#[derive(Default)]
struct Catalog {
    assets: BTreeMap<String, AssetView>,
    /// Launches read from each chain's factory, by hub token.
    candidates: BTreeMap<String, Vec<Candidate>>,
    /// How many launches have been read from each chain's factory.
    scanned: BTreeMap<String, u64>,
    refreshed_at: u64,
}

struct Chain {
    kind: String,
    domain: u32,
    rpc: Option<Rpc>,
}

pub struct Trade {
    config: TradeConfig,
    chains: BTreeMap<String, Chain>,
    catalog: RwLock<Catalog>,
    /// One refresh at a time: two would read the same new launches twice.
    refreshing: tokio::sync::Mutex<()>,
    http: reqwest::Client,
}

/// How often launches and pools are re-read.
const REFRESH: Duration = Duration::from_secs(30);

impl Trade {
    /// The trade desk the config describes, or none when it has no `[trade]`.
    pub fn new(config: &Config) -> Result<Option<Self>> {
        let Some(trade) = config.trade.clone() else {
            return Ok(None);
        };
        let mut chains = BTreeMap::new();
        let names = trade
            .assets
            .values()
            .flat_map(|a| a.routers.keys().cloned())
            .chain(trade.venues.keys().cloned())
            .chain([trade.hub.clone()]);
        for name in names {
            if chains.contains_key(&name) {
                continue;
            }
            let table = config.chains.get(&name).with_context(|| {
                format!("[trade] names chain `{name}`, which is not configured")
            })?;
            let kind = config.kind(&name)?.to_string();
            let url = |k: &str| table.get(k).and_then(|v| v.as_str());
            // The free endpoints first: the metered archive is for the ISM's trusted height only.
            let rpc = (kind != "celestia")
                .then(|| {
                    trade
                        .venues
                        .get(&name)
                        .and_then(|v| v.rpc.as_deref())
                        .or(url("send_rpc"))
                        .or(url("logs_rpc"))
                        .or(url("rpc"))
                })
                .flatten()
                .map(|u| Rpc::new(u, None));
            chains.insert(
                name.clone(),
                Chain {
                    kind,
                    domain: config.domain(&name)?,
                    rpc,
                },
            );
        }
        if config.kind(&trade.hub)? != "celestia" {
            bail!("[trade] hub `{}` is not a Celestia chain", trade.hub);
        }
        if !trade.venues.contains_key(&trade.default_venue) {
            bail!(
                "[trade] default_venue `{}` has no venue",
                trade.default_venue
            );
        }
        if !trade.assets.contains_key(&trade.quote_asset) {
            bail!(
                "[trade] quote_asset `{}` is not an asset",
                trade.quote_asset
            );
        }
        let assets = trade
            .assets
            .iter()
            .map(|(symbol, a)| {
                (
                    symbol.clone(),
                    AssetView {
                        id: symbol.clone(),
                        symbol: symbol.clone(),
                        name: symbol.clone(),
                        decimals: a.decimals,
                        denom: a.denom.clone(),
                        routers: a.routers.clone(),
                        launched: false,
                        launcher: None,
                        pools: BTreeMap::new(),
                    },
                )
            })
            .collect();
        Ok(Some(Self {
            config: trade,
            chains,
            catalog: RwLock::new(Catalog {
                assets,
                ..Default::default()
            }),
            refreshing: Default::default(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
        }))
    }

    /// Keep the catalog fresh for as long as the API runs.
    pub fn spawn_refresh(self: &Arc<Self>) {
        let trade = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = trade.refresh().await {
                    warn!(error = %e, "trade catalog refresh failed");
                }
                tokio::time::sleep(REFRESH).await;
            }
        });
    }

    // ------------------------------------------------------------ discovery

    async fn refresh(&self) -> Result<()> {
        let _one = self.refreshing.lock().await;
        self.scan_factories().await?;
        self.verify_launches().await;
        self.read_pools().await;
        self.catalog.write().await.refreshed_at = crate::route::now();
        Ok(())
    }

    /// Read new launches from every token factory.
    /// Read new launches from every token factory. A chain that fails is retried on the next
    /// refresh without holding up the others.
    async fn scan_factories(&self) -> Result<()> {
        for (chain, venue) in &self.config.venues {
            let Some(factory) = &venue.token_factory else {
                continue;
            };
            if let Err(e) = self.scan_factory(chain, factory).await {
                warn!(chain = %chain, error = %e, "reading the token factory failed");
            }
        }
        Ok(())
    }

    async fn scan_factory(&self, chain: &str, factory: &str) -> Result<()> {
        let count = word_u128(
            &self
                .eth_call(chain, factory, &call("launchCount()", &[]))
                .await?,
            0,
        )? as u64;
        let from = *self.catalog.read().await.scanned.get(chain).unwrap_or(&0);
        for i in from..count {
            let raw = self
                .eth_call(
                    chain,
                    factory,
                    &call("launches(uint256)", &[uint_word(i as u128)]),
                )
                .await?;
            let router = word_address(&raw, 0)?;
            let hub = format!(
                "0x{}",
                hex::encode(raw.get(32..64).context("short launch")?)
            );
            let launcher = word_address(&raw, 2)?;
            let name = decode_string(&self.eth_call(chain, &router, &call("name()", &[])).await?)?;
            let symbol = decode_string(
                &self
                    .eth_call(chain, &router, &call("symbol()", &[]))
                    .await?,
            )?;
            let mut catalog = self.catalog.write().await;
            catalog.candidates.entry(hub).or_default().push(Candidate {
                chain: chain.to_string(),
                router,
                launcher,
                name,
                symbol,
            });
            catalog.scanned.insert(chain.to_string(), i + 1);
        }
        Ok(())
    }

    /// List every launch whose hub token is now wired as the venue requires. Once true it
    /// stays true: the token's owner has renounced, so nothing about it can change.
    async fn verify_launches(&self) {
        let pending: Vec<(String, Vec<Candidate>)> = {
            let catalog = self.catalog.read().await;
            catalog
                .candidates
                .iter()
                .filter(|(hub, _)| !catalog.assets.contains_key(*hub))
                .map(|(h, c)| (h.clone(), c.clone()))
                .collect()
        };
        for (hub, candidates) in pending {
            match self.check_launch(&hub, &candidates).await {
                Ok(Some(asset)) => {
                    info!(token = %hub, symbol = %asset.symbol, "launched token listed");
                    for (chain, router) in &asset.routers {
                        if let (Some(c), Some(bytes)) =
                            (self.chains.get(chain), crate::registry::to_32(router))
                        {
                            crate::registry::add(c.domain, bytes);
                        }
                    }
                    self.catalog.write().await.assets.insert(hub, asset);
                }
                Ok(None) => {}
                Err(e) => debug!(token = %hub, error = %e, "launch not checkable yet"),
            }
        }
    }

    /// The listing for `hub`, or none if its wiring is not (yet) what the venue requires.
    async fn check_launch(&self, hub: &str, candidates: &[Candidate]) -> Result<Option<AssetView>> {
        let token: Value = self.rest(&format!("/hyperlane/v1/tokens/{hub}")).await?;
        let token = &token["token"];
        let same = |a: &Value, b: &str| a.as_str().is_some_and(|a| a.eq_ignore_ascii_case(b));
        let wired = token["token_type"] == "HYP_TOKEN_TYPE_SYNTHETIC"
            && token["owner"].as_str() == Some("")
            && same(&token["origin_mailbox"], &self.config.hub_mailbox)
            && same(&token["ism_id"], &self.config.routing_ism);
        if !wired {
            return Ok(None);
        }
        let enrolled: Value = self
            .rest(&format!("/hyperlane/v1/tokens/{hub}/remote_routers"))
            .await?;
        let mut routers = BTreeMap::from([(self.config.hub.clone(), hub.to_string())]);
        let mut first: Option<&Candidate> = None;
        for r in enrolled["remote_routers"].as_array().into_iter().flatten() {
            let domain = r["receiver_domain"].as_u64().unwrap_or(0) as u32;
            let contract = r["receiver_contract"].as_str().unwrap_or("");
            // Every router the token trusts has to be one a factory made, on a chain with one.
            let Some(c) = candidates.iter().find(|c| {
                self.chains
                    .get(&c.chain)
                    .is_some_and(|ch| ch.domain == domain)
                    && crate::registry::to_32(&c.router) == crate::registry::to_32(contract)
            }) else {
                return Ok(None);
            };
            first.get_or_insert(c);
            routers.insert(c.chain.clone(), c.router.clone());
        }
        let Some(c) = first else {
            return Ok(None);
        };
        Ok(Some(AssetView {
            id: hub.to_string(),
            symbol: c.symbol.clone(),
            name: c.name.clone(),
            decimals: 6,
            denom: format!("hyperlane/{hub}"),
            routers,
            launched: true,
            launcher: Some(c.launcher.clone()),
            pools: BTreeMap::new(),
        }))
    }

    /// Which pools against the quote asset exist and hold liquidity.
    async fn read_pools(&self) {
        let assets: Vec<AssetView> = self.catalog.read().await.assets.values().cloned().collect();
        let quote = self.config.quote_asset.clone();
        for asset in assets.iter().filter(|a| a.id != quote) {
            let mut pools = BTreeMap::new();
            for chain in self.config.venues.keys() {
                if let Ok(Some(pool)) = self.live_pool(chain, &asset.id, &quote, &assets).await {
                    pools.insert(chain.clone(), pool);
                }
            }
            if let Some(a) = self.catalog.write().await.assets.get_mut(&asset.id) {
                a.pools = pools;
            }
        }
    }

    /// The pool for a pair on `chain`, if it exists and holds liquidity.
    async fn live_pool(
        &self,
        chain: &str,
        a: &str,
        b: &str,
        assets: &[AssetView],
    ) -> Result<Option<String>, ApiError> {
        let find = |id: &str| {
            assets
                .iter()
                .find(|x| x.id == id)
                .and_then(|x| x.routers.get(chain))
        };
        let (Some(ta), Some(tb)) = (find(a), find(b)) else {
            return Ok(None);
        };
        let pool = self.pool_address(chain, ta, tb).await?;
        let Some(pool) = pool else {
            return Ok(None);
        };
        let liquidity = word_u128(
            &self
                .eth_call(chain, &pool, &call("liquidity()", &[]))
                .await?,
            0,
        )?;
        Ok((liquidity > 0).then_some(pool))
    }

    async fn pool_address(
        &self,
        chain: &str,
        ta: &str,
        tb: &str,
    ) -> Result<Option<String>, ApiError> {
        let venue = self.venue(chain)?;
        let raw = self
            .eth_call(
                chain,
                &venue.factory,
                &call(
                    "getPool(address,address,uint24)",
                    &[
                        address_word(ta)?,
                        address_word(tb)?,
                        uint_word(venue.fee as u128),
                    ],
                ),
            )
            .await?;
        let pool = word_address(&raw, 0).map_err(|e| upstream(e.to_string()))?;
        Ok((pool != format!("0x{}", "0".repeat(40))).then_some(pool))
    }

    async fn rest(&self, path: &str) -> Result<Value> {
        let url = format!("{}{path}", self.config.hub_rest.trim_end_matches('/'));
        let reply = self.http.get(&url).send().await?;
        let status = reply.status();
        let body: Value = reply
            .json()
            .await
            .with_context(|| format!("{url}: not JSON"))?;
        if !status.is_success() {
            bail!("{url}: {status} {body}");
        }
        Ok(body)
    }

    // ------------------------------------------------------------ lookups

    async fn asset(&self, id: &str) -> Result<AssetView, ApiError> {
        let catalog = self.catalog.read().await;
        if let Some(a) = catalog
            .assets
            .get(id)
            .or_else(|| catalog.assets.get(&id.to_lowercase()))
        {
            return Ok(a.clone());
        }
        let mut by_symbol = catalog.assets.values().filter(|a| a.symbol == id);
        match (by_symbol.next(), by_symbol.next()) {
            (Some(a), None) => Ok(a.clone()),
            (Some(_), Some(_)) => Err(bad(format!("`{id}` names more than one token; use its id"))),
            _ => Err(bad(format!("unknown asset `{id}`"))),
        }
    }

    fn chain(&self, name: &str) -> Result<&Chain, ApiError> {
        self.chains
            .get(name)
            .ok_or_else(|| bad(format!("unknown chain `{name}`")))
    }

    fn venue(&self, chain: &str) -> Result<&Venue, ApiError> {
        self.config
            .venues
            .get(chain)
            .ok_or_else(|| bad(format!("no pools on {chain}")))
    }

    fn rpc(&self, chain: &str) -> Result<&Rpc, ApiError> {
        self.chain(chain)?
            .rpc
            .as_ref()
            .ok_or_else(|| bad(format!("{chain} is not an EVM chain")))
    }

    async fn eth_call(&self, chain: &str, to: &str, data: &[u8]) -> Result<Vec<u8>, ApiError> {
        let result = self
            .rpc(chain)?
            .read(
                "eth_call",
                json!([{ "to": to, "data": format!("0x{}", hex::encode(data)) }, "latest"]),
            )
            .await
            .map_err(|e| upstream(format!("{chain}: {e}")))?;
        let text = result.as_str().unwrap_or_default().trim_start_matches("0x");
        hex::decode(text).map_err(|_| upstream(format!("{chain}: eth_call returned {result}")))
    }

    // ------------------------------------------------------------ routes

    /// The steps, without amounts.
    async fn plan(
        &self,
        from: &str,
        sell: &AssetView,
        to: &str,
        buy: &AssetView,
    ) -> Result<Vec<Step>, ApiError> {
        for (asset, chain) in [(sell, from), (buy, to)] {
            if !asset.routers.contains_key(chain) {
                return Err(bad(format!("{} is not on {chain}", asset.symbol)));
            }
        }
        let mut steps = Vec::new();
        if sell.id == buy.id {
            self.bridges(&mut steps, from, to, sell)?;
        } else {
            let mut order = vec![
                from.to_string(),
                to.to_string(),
                self.config.default_venue.clone(),
            ];
            order.extend(self.config.venues.keys().cloned());
            let mut venue = None;
            for chain in order {
                if self.config.venues.contains_key(&chain)
                    && sell.routers.contains_key(&chain)
                    && buy.routers.contains_key(&chain)
                    && self
                        .best_path(&chain, &sell.id, &buy.id, None)
                        .await
                        .is_ok()
                {
                    venue = Some(chain);
                    break;
                }
            }
            let venue = venue.ok_or_else(|| {
                bad(format!(
                    "no chain has pools to swap {} for {}",
                    sell.symbol, buy.symbol
                ))
            })?;
            self.bridges(&mut steps, from, &venue, sell)?;
            steps.push(Step::new(StepKind::Swap, &venue, &venue, &sell.id, &buy.id));
            self.bridges(&mut steps, &venue, to, buy)?;
        }
        if steps.is_empty() {
            return Err(bad("nothing to do: same asset on the same chain".into()));
        }
        Ok(steps)
    }

    /// One bridge to or from the hub, or two through it.
    fn bridges(
        &self,
        steps: &mut Vec<Step>,
        from: &str,
        to: &str,
        asset: &AssetView,
    ) -> Result<(), ApiError> {
        if from == to {
            return Ok(());
        }
        let hub = self.config.hub.as_str();
        let hops: Vec<(&str, &str)> = if from == hub || to == hub {
            vec![(from, to)]
        } else {
            vec![(from, hub), (hub, to)]
        };
        for (a, b) in hops {
            for c in [a, b] {
                if !asset.routers.contains_key(c) {
                    return Err(bad(format!("{} is not on {c}", asset.symbol)));
                }
            }
            let mut step = Step::new(StepKind::Bridge, a, b, &asset.id, &asset.id);
            step.eta_secs = typical_latency(&self.chain(a)?.kind);
            steps.push(step);
        }
        Ok(())
    }

    /// The token path for a swap on `chain` and what it pays for `amount`: the direct pool or
    /// the route through the quote asset, whichever pays more. Without an amount, any that works.
    async fn best_path(
        &self,
        chain: &str,
        sell: &str,
        buy: &str,
        amount: Option<u128>,
    ) -> Result<(Vec<String>, u128), ApiError> {
        let assets: Vec<AssetView> = self.catalog.read().await.assets.values().cloned().collect();
        let quote = &self.config.quote_asset;
        let mut paths = vec![vec![sell.to_string(), buy.to_string()]];
        if sell != quote && buy != quote {
            paths.push(vec![sell.to_string(), quote.clone(), buy.to_string()]);
        }
        let mut best: Option<(Vec<String>, u128)> = None;
        for path in paths {
            let mut live = true;
            for pair in path.windows(2) {
                if self
                    .live_pool(chain, &pair[0], &pair[1], &assets)
                    .await?
                    .is_none()
                {
                    live = false;
                    break;
                }
            }
            if !live {
                continue;
            }
            let out = match amount {
                Some(a) => self.quote_path(chain, &path, a, &assets).await?,
                None => return Ok((path, 0)),
            };
            if best.as_ref().is_none_or(|(_, o)| out > *o) {
                best = Some((path, out));
            }
        }
        best.ok_or_else(|| bad(format!("no pool on {chain} connects these assets")))
    }

    fn encode_path(
        &self,
        chain: &str,
        path: &[String],
        assets: &[AssetView],
    ) -> Result<Vec<u8>, ApiError> {
        let fee = self.venue(chain)?.fee;
        let mut out = Vec::new();
        for (i, id) in path.iter().enumerate() {
            let token = assets
                .iter()
                .find(|a| &a.id == id)
                .and_then(|a| a.routers.get(chain))
                .ok_or_else(|| bad(format!("{id} is not on {chain}")))?;
            out.extend_from_slice(&address_word(token)?[12..]);
            if i + 1 < path.len() {
                out.extend_from_slice(&fee.to_be_bytes()[1..]);
            }
        }
        Ok(out)
    }

    async fn quote_path(
        &self,
        chain: &str,
        path: &[String],
        amount: u128,
        assets: &[AssetView],
    ) -> Result<u128, ApiError> {
        let venue = self.venue(chain)?;
        let encoded = self.encode_path(chain, path, assets)?;
        // quoteExactInput(bytes path, uint256 amountIn)
        let mut data = selector("quoteExactInput(bytes,uint256)").to_vec();
        data.extend_from_slice(&uint_word(64));
        data.extend_from_slice(&uint_word(amount));
        data.extend_from_slice(&dynamic_bytes(&encoded));
        let out = self.eth_call(chain, &venue.quoter, &data).await?;
        word_u128(&out, 0)
    }

    async fn quote(&self, q: QuoteQuery) -> Result<QuoteView, ApiError> {
        let amount = parse_amount(&q.amount)?;
        let (sell, buy) = (self.asset(&q.sell).await?, self.asset(&q.buy).await?);
        let mut steps = self.plan(&q.from, &sell, &q.to, &buy).await?;
        let mut running = amount;
        for step in &mut steps {
            step.amount_in = running.to_string();
            if step.kind == StepKind::Swap {
                running = self
                    .best_path(&step.from, &step.sell, &step.buy, Some(running))
                    .await?
                    .1;
            }
            step.amount_out = running.to_string();
        }
        Ok(QuoteView {
            from: q.from,
            to: q.to,
            sell: sell.id,
            buy: buy.id,
            amount_in: amount.to_string(),
            amount_out: running.to_string(),
            eta_secs: steps.iter().map(|s| s.eta_secs).sum(),
            steps,
        })
    }

    // ------------------------------------------------------------ transactions

    async fn build(&self, req: BuildRequest) -> Result<BuildView, ApiError> {
        let step = &req.step;
        let amount = parse_amount(req.amount.as_deref().unwrap_or(&step.amount_in))?;
        let assets: Vec<AssetView> = self.catalog.read().await.assets.values().cloned().collect();
        match step.kind {
            StepKind::Swap => {
                let chain = step.from.as_str();
                let venue = self.venue(chain)?;
                let (path, quoted) = self
                    .best_path(chain, &step.sell, &step.buy, Some(amount))
                    .await?;
                let sell = self.asset(&step.sell).await?;
                let sell_token = sell
                    .routers
                    .get(chain)
                    .ok_or_else(|| bad(format!("{} is not on {chain}", sell.symbol)))?;
                let slippage = req.slippage_bps.unwrap_or(50).min(5000) as u128;
                let min_out = quoted * (10_000 - slippage) / 10_000;
                let mut txs = Vec::new();
                if let Some(tx) = self
                    .approval(
                        chain,
                        sell_token,
                        &req.sender,
                        &venue.swap_router,
                        amount,
                        &sell.symbol,
                    )
                    .await?
                {
                    txs.push(tx);
                }
                // exactInput((bytes path, address recipient, uint256 amountIn, uint256 amountOutMinimum))
                let mut data = selector("exactInput((bytes,address,uint256,uint256))").to_vec();
                data.extend_from_slice(&uint_word(32));
                data.extend_from_slice(&uint_word(128));
                data.extend_from_slice(&address_word(&req.sender)?);
                data.extend_from_slice(&uint_word(amount));
                data.extend_from_slice(&uint_word(min_out));
                data.extend_from_slice(&dynamic_bytes(&self.encode_path(chain, &path, &assets)?));
                txs.push(self.evm_tx(chain, &venue.swap_router, data, 0, "Swap".into())?);
                Ok(BuildView {
                    txs,
                    amount_out: quoted.to_string(),
                    min_out: min_out.to_string(),
                })
            }
            StepKind::Bridge => {
                let recipient = req.recipient.as_deref().ok_or_else(|| {
                    bad("a bridge step needs `recipient` on the destination".into())
                })?;
                let asset = self.asset(&step.sell).await?;
                let tx = self
                    .bridge_tx(&step.from, &step.to, &asset, amount, &req.sender, recipient)
                    .await?;
                Ok(BuildView {
                    txs: vec![tx],
                    amount_out: amount.to_string(),
                    min_out: amount.to_string(),
                })
            }
        }
    }

    async fn bridge_tx(
        &self,
        from: &str,
        to: &str,
        asset: &AssetView,
        amount: u128,
        sender: &str,
        recipient: &str,
    ) -> Result<Tx, ApiError> {
        let destination = self.chain(to)?.domain;
        let router = asset
            .routers
            .get(from)
            .ok_or_else(|| bad(format!("{} is not on {from}", asset.symbol)))?;
        let recipient32 = bytes32(recipient)?;
        let description = format!("Bridge {} to {to}", asset.symbol);
        if from == self.config.hub {
            return Ok(self.cosmos_tx(
                vec![msg(
                    "/hyperlane.warp.v1.MsgRemoteTransfer",
                    json!({
                        "sender": sender,
                        "token_id": router,
                        "destination_domain": destination,
                        "recipient": hex32(&recipient32),
                        "amount": amount.to_string(),
                        "custom_hook_id": "",
                        "gas_limit": "0",
                        "max_fee": { "denom": "utia", "amount": self.config.celestia_max_fee.to_string() },
                    }),
                )],
                description,
            ));
        }
        let fee = self
            .eth_call(
                from,
                router,
                &call("quoteGasPayment(uint32)", &[uint_word(destination as u128)]),
            )
            .await?;
        self.evm_tx(
            from,
            router,
            call(
                "transferRemote(uint32,bytes32,uint256)",
                &[
                    uint_word(destination as u128),
                    recipient32,
                    uint_word(amount),
                ],
            ),
            word_u128(&fee, 0)?,
            description,
        )
    }

    /// An approval of `amount` for `spender`, unless one is already in place.
    async fn approval(
        &self,
        chain: &str,
        token: &str,
        owner: &str,
        spender: &str,
        amount: u128,
        symbol: &str,
    ) -> Result<Option<Tx>, ApiError> {
        let allowance = self
            .eth_call(
                chain,
                token,
                &call(
                    "allowance(address,address)",
                    &[address_word(owner)?, address_word(spender)?],
                ),
            )
            .await?;
        if word_u128(&allowance, 0).unwrap_or(0) >= amount {
            return Ok(None);
        }
        Ok(Some(self.evm_tx(
            chain,
            token,
            call(
                "approve(address,uint256)",
                &[address_word(spender)?, uint_word(amount)],
            ),
            0,
            format!("Allow {symbol} to be spent"),
        )?))
    }

    fn evm_tx(
        &self,
        chain: &str,
        to: &str,
        data: Vec<u8>,
        value: u128,
        description: String,
    ) -> Result<Tx, ApiError> {
        Ok(Tx {
            chain: chain.to_string(),
            kind: "evm".into(),
            // Hyperlane's domain for an EVM chain is its chain id.
            chain_id: Some(self.chain(chain)?.domain as u64),
            to: Some(to.to_string()),
            data: Some(format!("0x{}", hex::encode(data))),
            value: Some(format!("0x{value:x}")),
            msgs: None,
            description,
        })
    }

    fn cosmos_tx(&self, msgs: Vec<CosmosMsg>, description: String) -> Tx {
        Tx {
            chain: self.config.hub.clone(),
            kind: "cosmos".into(),
            chain_id: None,
            to: None,
            data: None,
            value: None,
            msgs: Some(msgs),
            description,
        }
    }

    // ------------------------------------------------------------ liquidity

    /// Add full-range liquidity to the pool for a pair, creating it at the price the two
    /// amounts imply when it does not exist yet. On an existing pool `amountB` is computed
    /// from its price.
    async fn pool(&self, req: PoolRequest) -> Result<PoolView, ApiError> {
        let chain = req.chain.as_str();
        let venue = self.venue(chain)?.clone();
        let (a, b) = (
            self.asset(&req.asset_a).await?,
            self.asset(&req.asset_b).await?,
        );
        if a.id == b.id {
            return Err(bad("a pool needs two different assets".into()));
        }
        let token = |x: &AssetView| {
            x.routers
                .get(chain)
                .cloned()
                .ok_or_else(|| bad(format!("{} is not on {chain}", x.symbol)))
        };
        let (ta, tb) = (token(&a)?, token(&b)?);
        let amount_a = parse_amount(&req.amount_a)?;
        let existing = self.pool_address(chain, &ta, &tb).await?;
        let sqrt_price = match &existing {
            Some(pool) => {
                let slot0 = self.eth_call(chain, pool, &call("slot0()", &[])).await?;
                U256::from_be_slice(
                    slot0
                        .get(..32)
                        .ok_or_else(|| upstream("short slot0".into()))?,
                )
            }
            None => U256::ZERO,
        };
        // Uniswap orders a pair by address, and quotes token1 per token0.
        let a_first = address_word(&ta)? < address_word(&tb)?;
        let amount_b =
            if sqrt_price.is_zero() {
                parse_amount(req.amount_b.as_deref().ok_or_else(|| {
                    bad("a new pool needs `amountB`, which sets its price".into())
                })?)?
            } else {
                amount_at_price(amount_a, sqrt_price, a_first)?
            };
        let (t0, t1, a0, a1) = if a_first {
            (&ta, &tb, amount_a, amount_b)
        } else {
            (&tb, &ta, amount_b, amount_a)
        };
        let slippage = req.slippage_bps.unwrap_or(100).min(5000) as u128;
        let spacing = match venue.fee {
            100 => 1,
            500 => 10,
            10_000 => 200,
            _ => 60,
        };
        let tick = 887_272 / spacing * spacing;
        let mut txs = Vec::new();
        for (t, amount, x) in [(&ta, amount_a, &a), (&tb, amount_b, &b)] {
            if let Some(tx) = self
                .approval(chain, t, &req.sender, &venue.positions, amount, &x.symbol)
                .await?
            {
                txs.push(tx);
            }
        }
        // mint((token0, token1, fee, tickLower, tickUpper, amount0Desired, amount1Desired,
        //       amount0Min, amount1Min, recipient, deadline))
        let mint = call(
            "mint((address,address,uint24,int24,int24,uint256,uint256,uint256,uint256,address,uint256))",
            &[
                address_word(t0)?,
                address_word(t1)?,
                uint_word(venue.fee as u128),
                int_word(-(tick as i64)),
                int_word(tick as i64),
                uint_word(a0),
                uint_word(a1),
                uint_word(a0 * (10_000 - slippage) / 10_000),
                uint_word(a1 * (10_000 - slippage) / 10_000),
                address_word(&req.sender)?,
                uint_word((crate::route::now() + 1800) as u128),
            ],
        );
        let data = if existing.is_some() {
            mint
        } else {
            let price = (U512::from(a1) << 192usize) / U512::from(a0);
            let sqrt = price.root(2).to::<U256>();
            let mut word = [0u8; 32];
            word.copy_from_slice(&sqrt.to_be_bytes::<32>());
            let create = call(
                "createAndInitializePoolIfNecessary(address,address,uint24,uint160)",
                &[
                    address_word(t0)?,
                    address_word(t1)?,
                    uint_word(venue.fee as u128),
                    word,
                ],
            );
            multicall(&[create, mint])
        };
        let description = format!(
            "{} the {}/{} pool on {chain}",
            if existing.is_some() {
                "Add to"
            } else {
                "Create"
            },
            a.symbol,
            b.symbol
        );
        txs.push(self.evm_tx(chain, &venue.positions, data, 0, description)?);
        Ok(PoolView {
            txs,
            pool: existing,
            amount_a: amount_a.to_string(),
            amount_b: amount_b.to_string(),
        })
    }

    // ------------------------------------------------------------ launching

    fn launch_create(&self, req: CreateRequest) -> Tx {
        self.cosmos_tx(
            vec![msg(
                "/hyperlane.warp.v1.MsgCreateSyntheticToken",
                json!({ "owner": req.owner, "origin_mailbox": self.config.hub_mailbox }),
            )],
            "Create the token on the hub".into(),
        )
    }

    fn launch_deploy(&self, req: DeployRequest) -> Result<Vec<Tx>, ApiError> {
        let hub = parse_hex32(&req.hub_token)?;
        if req.symbol.is_empty()
            || req.symbol.len() > 12
            || req.name.is_empty()
            || req.name.len() > 48
        {
            return Err(bad(
                "a symbol of 1 to 12 characters and a name of 1 to 48".into()
            ));
        }
        let mut txs = Vec::new();
        for (chain, venue) in &self.config.venues {
            if req.chains.as_ref().is_some_and(|c| !c.contains(chain)) {
                continue;
            }
            let Some(factory) = &venue.token_factory else {
                continue;
            };
            // launch(bytes32 hubToken, string name, string symbol)
            let mut data = selector("launch(bytes32,string,string)").to_vec();
            let name = dynamic_bytes(req.name.as_bytes());
            data.extend_from_slice(&hub);
            data.extend_from_slice(&uint_word(96));
            data.extend_from_slice(&uint_word(96 + name.len() as u128));
            data.extend_from_slice(&name);
            data.extend_from_slice(&dynamic_bytes(req.symbol.as_bytes()));
            txs.push(self.evm_tx(
                chain,
                factory,
                data,
                0,
                format!("Launch {} on {chain}", req.symbol),
            )?);
        }
        Ok(txs)
    }

    /// The hub transaction that mints the whole supply once, trusts the routers, and gives
    /// the token up: after it, nobody can mint more or change what it trusts.
    async fn launch_wire(&self, req: WireRequest) -> Result<Tx, ApiError> {
        let hub = parse_hex32(&req.hub_token)?;
        let hub_hex = hex32(&hub);
        let supply = parse_amount(&req.supply)?;
        let owner = crate::bech32::decode(&req.owner)
            .filter(|(_, b)| b.len() == 20)
            .ok_or_else(|| bad(format!("`{}` is not a bech32 account", req.owner)))?
            .1;
        let candidates = self
            .catalog
            .read()
            .await
            .candidates
            .get(&hub_hex)
            .cloned()
            .unwrap_or_default();
        let mut routers: BTreeMap<String, String> = BTreeMap::new();
        for c in candidates
            .iter()
            .filter(|c| c.launcher.eq_ignore_ascii_case(&req.launcher))
        {
            routers.insert(c.chain.clone(), c.router.clone());
        }
        if routers.is_empty() {
            return Err(bad(format!(
                "no launch of {hub_hex} by {} found yet; deploy first, then wait for the next refresh",
                req.launcher
            )));
        }
        let hub_domain = self.chain(&self.config.hub)?.domain;
        let message = mint_message(hub_domain, &hub, &owner, supply);
        let mut msgs = vec![
            msg(
                "/hyperlane.warp.v1.MsgSetToken",
                json!({ "owner": req.owner, "token_id": hub_hex, "new_owner": "", "ism_id": self.config.noop_ism, "renounce_ownership": false }),
            ),
            msg(
                "/hyperlane.warp.v1.MsgEnrollRemoteRouter",
                json!({ "owner": req.owner, "token_id": hub_hex, "remote_router": { "receiver_domain": MINT_DOMAIN, "receiver_contract": hex32(&MINT_ROUTER), "gas": "0" } }),
            ),
            msg(
                "/hyperlane.core.v1.MsgProcessMessage",
                json!({ "mailbox_id": self.config.hub_mailbox, "relayer": req.owner, "metadata": "0x", "message": format!("0x{}", hex::encode(&message)) }),
            ),
            msg(
                "/hyperlane.warp.v1.MsgUnrollRemoteRouter",
                json!({ "owner": req.owner, "token_id": hub_hex, "receiver_domain": MINT_DOMAIN }),
            ),
        ];
        for (chain, router) in &routers {
            msgs.push(msg(
                "/hyperlane.warp.v1.MsgEnrollRemoteRouter",
                json!({ "owner": req.owner, "token_id": hub_hex, "remote_router": {
                    "receiver_domain": self.chain(chain)?.domain,
                    "receiver_contract": hex32(&address_word(router)?),
                    "gas": self.config.router_gas.to_string(),
                } }),
            ));
        }
        msgs.push(msg(
            "/hyperlane.warp.v1.MsgSetToken",
            json!({ "owner": req.owner, "token_id": hub_hex, "new_owner": "", "ism_id": self.config.routing_ism, "renounce_ownership": true }),
        ));
        Ok(self.cosmos_tx(
            msgs,
            "Mint the supply, connect the chains and renounce the token".into(),
        ))
    }

    async fn launch_status(&self, hub: &str) -> Result<LaunchView, ApiError> {
        let hub = hex32(&parse_hex32(hub)?);
        let catalog = self.catalog.read().await;
        Ok(LaunchView {
            listed: catalog.assets.get(&hub).cloned(),
            launches: catalog.candidates.get(&hub).cloned().unwrap_or_default(),
            refreshed_at: catalog.refreshed_at,
            id: hub,
        })
    }

    async fn info(&self) -> TradeInfo {
        let catalog = self.catalog.read().await;
        TradeInfo {
            hub: self.config.hub.clone(),
            quote_asset: self.config.quote_asset.clone(),
            refreshed_at: catalog.refreshed_at,
            chains: self
                .chains
                .iter()
                .map(|(name, c)| TradeChain {
                    name: name.clone(),
                    kind: if c.kind == "celestia" {
                        "cosmos"
                    } else {
                        "evm"
                    }
                    .into(),
                    domain: c.domain,
                    venue: self.config.venues.get(name).cloned(),
                })
                .collect(),
            assets: catalog.assets.values().cloned().collect(),
        }
    }
}

/// The made-up origin a launch mints its supply from, once, before it unrolls it: "teeU".
const MINT_DOMAIN: u32 = 1_952_802_133;
const MINT_ROUTER: [u8; 32] = {
    let mut r = [0u8; 32];
    r[28] = 0x74;
    r[29] = 0x65;
    r[30] = 0x65;
    r[31] = 0x55;
    r
};

/// version 3 | nonce 0 | origin | sender | destination | recipient | body (recipient, amount)
fn mint_message(hub_domain: u32, token: &[u8; 32], owner: &[u8], amount: u128) -> Vec<u8> {
    let mut m = vec![3u8];
    m.extend_from_slice(&0u32.to_be_bytes());
    m.extend_from_slice(&MINT_DOMAIN.to_be_bytes());
    m.extend_from_slice(&MINT_ROUTER);
    m.extend_from_slice(&hub_domain.to_be_bytes());
    m.extend_from_slice(token);
    m.extend_from_slice(&[0u8; 12]);
    m.extend_from_slice(owner);
    m.extend_from_slice(&uint_word(amount));
    m
}

/// How much of the other token a full-range position at this price takes for `amount`.
fn amount_at_price(
    amount: u128,
    sqrt_price: U256,
    amount_is_token0: bool,
) -> Result<u128, ApiError> {
    let q192 = U512::from(1u8) << 192usize;
    let p2 = U512::from(sqrt_price) * U512::from(sqrt_price);
    let out = if amount_is_token0 {
        U512::from(amount) * p2 / q192
    } else {
        U512::from(amount) * q192 / p2
    };
    u128::try_from(out).map_err(|_| bad("that amount is too large for this pool".into()))
}

/// How long a bridge leaving a chain of this kind usually takes, origin finality plus one
/// attestation and delivery. The bridge app shows the same numbers.
fn typical_latency(kind: &str) -> u64 {
    45 + match kind {
        "celestia" => 60,
        "ethereum" => 13 * 60,
        "eden" => 3 * 60,
        _ => 30,
    }
}

// ---------------------------------------------------------------- ABI, by hand

fn selector(signature: &str) -> [u8; 4] {
    keccak256(signature.as_bytes())[..4]
        .try_into()
        .expect("4 bytes")
}

/// Selector and static words.
fn call(signature: &str, words: &[[u8; 32]]) -> Vec<u8> {
    let mut out = selector(signature).to_vec();
    for w in words {
        out.extend_from_slice(w);
    }
    out
}

/// Length, then the bytes padded to a word.
fn dynamic_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = uint_word(data.len() as u128).to_vec();
    out.extend_from_slice(data);
    out.resize(32 + data.len().div_ceil(32) * 32, 0);
    out
}

/// `multicall(bytes[])`.
fn multicall(calls: &[Vec<u8>]) -> Vec<u8> {
    let mut out = selector("multicall(bytes[])").to_vec();
    out.extend_from_slice(&uint_word(32));
    out.extend_from_slice(&uint_word(calls.len() as u128));
    let encoded: Vec<Vec<u8>> = calls.iter().map(|c| dynamic_bytes(c)).collect();
    let mut offset = 32 * calls.len();
    for e in &encoded {
        out.extend_from_slice(&uint_word(offset as u128));
        offset += e.len();
    }
    for e in encoded {
        out.extend_from_slice(&e);
    }
    out
}

fn uint_word(n: u128) -> [u8; 32] {
    let mut w = [0; 32];
    w[16..].copy_from_slice(&n.to_be_bytes());
    w
}

fn int_word(n: i64) -> [u8; 32] {
    let mut w = if n < 0 { [0xff; 32] } else { [0; 32] };
    w[24..].copy_from_slice(&n.to_be_bytes());
    w
}

fn address_word(address: &str) -> Result<[u8; 32], ApiError> {
    let raw = hex::decode(address.trim_start_matches("0x"))
        .ok()
        .filter(|b| b.len() == 20)
        .ok_or_else(|| bad(format!("`{address}` is not an EVM address")))?;
    let mut w = [0; 32];
    w[12..].copy_from_slice(&raw);
    Ok(w)
}

fn parse_hex32(text: &str) -> Result<[u8; 32], ApiError> {
    hex::decode(text.trim_start_matches("0x"))
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| bad(format!("`{text}` is not a 32-byte hex id")))
}

fn hex32(bytes: &[u8; 32]) -> String {
    format!("0x{}", hex::encode(bytes))
}

/// A recipient as Hyperlane's 32 bytes: an EVM address left-padded, a bech32 account the same.
fn bytes32(recipient: &str) -> Result<[u8; 32], ApiError> {
    if recipient.starts_with("0x") {
        return address_word(recipient);
    }
    let (_, account) = crate::bech32::decode(recipient)
        .filter(|(_, b)| b.len() == 20)
        .ok_or_else(|| {
            bad(format!(
                "`{recipient}` is neither an EVM nor a bech32 address"
            ))
        })?;
    let mut w = [0; 32];
    w[12..].copy_from_slice(&account);
    Ok(w)
}

fn word_u128(data: &[u8], index: usize) -> Result<u128, ApiError> {
    let word = data
        .get(index * 32..index * 32 + 32)
        .ok_or_else(|| upstream("a short answer from the chain".into()))?;
    if word[..16].iter().any(|b| *b != 0) {
        return Err(upstream("an amount too large to handle".into()));
    }
    Ok(u128::from_be_bytes(
        word[16..].try_into().expect("16 bytes"),
    ))
}

fn word_address(data: &[u8], index: usize) -> Result<String> {
    let word = data
        .get(index * 32..index * 32 + 32)
        .context("a short answer from the chain")?;
    Ok(format!("0x{}", hex::encode(&word[12..])))
}

fn decode_string(data: &[u8]) -> Result<String> {
    let len = u128::from_be_bytes(data.get(48..64).context("short string")?.try_into()?) as usize;
    let bytes = data.get(64..64 + len).context("short string")?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn parse_amount(text: &str) -> Result<u128, ApiError> {
    match text.parse::<u128>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(bad(format!(
            "`{text}` is not a positive amount in base units"
        ))),
    }
}

fn msg(type_url: &str, value: Value) -> CosmosMsg {
    CosmosMsg {
        type_url: type_url.into(),
        value,
    }
}

// ---------------------------------------------------------------- types

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepKind {
    Swap,
    Bridge,
}

/// One step of a route. A swap has `from` and `to` both the chain it swaps on; a bridge has
/// `sell` and `buy` both the asset it moves. Assets are by id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub kind: StepKind,
    pub from: String,
    pub to: String,
    pub sell: String,
    pub buy: String,
    /// Base units, as decimal strings: amounts exceed what JSON numbers hold exactly.
    #[serde(default)]
    pub amount_in: String,
    #[serde(default)]
    pub amount_out: String,
    #[serde(default)]
    pub eta_secs: u64,
}

impl Step {
    fn new(kind: StepKind, from: &str, to: &str, sell: &str, buy: &str) -> Self {
        Self {
            kind,
            from: from.into(),
            to: to.into(),
            sell: sell.into(),
            buy: buy.into(),
            amount_in: String::new(),
            amount_out: String::new(),
            eta_secs: 0,
        }
    }
}

#[derive(Deserialize)]
pub struct QuoteQuery {
    from: String,
    sell: String,
    to: String,
    buy: String,
    /// Base units.
    amount: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteView {
    pub from: String,
    pub to: String,
    pub sell: String,
    pub buy: String,
    pub amount_in: String,
    pub amount_out: String,
    pub eta_secs: u64,
    pub steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildRequest {
    step: Step,
    /// What the caller actually holds for this step; the step's own `amountIn` when absent.
    amount: Option<String>,
    /// The signer: an EVM address, or a bech32 account for a step leaving the hub.
    sender: String,
    /// Who receives a bridge on the far side.
    recipient: Option<String>,
    /// How far below the quote a swap may fill. Default 50, so 0.5%.
    slippage_bps: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildView {
    /// In order. An approval, when needed, comes before its swap.
    pub txs: Vec<Tx>,
    pub amount_out: String,
    pub min_out: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CosmosMsg {
    pub type_url: String,
    /// The message in proto JSON field names.
    pub value: Value,
}

/// An unsigned transaction. `evm` fills `chainId`, `to`, `data` and `value`; `cosmos` fills
/// `msgs`, which go in one transaction in order.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tx {
    pub chain: String,
    pub kind: String,
    pub chain_id: Option<u64>,
    pub to: Option<String>,
    pub data: Option<String>,
    pub value: Option<String>,
    pub msgs: Option<Vec<CosmosMsg>>,
    pub description: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolRequest {
    chain: String,
    asset_a: String,
    asset_b: String,
    /// Base units of `assetA`.
    amount_a: String,
    /// Base units of `assetB`. Sets the price of a new pool; ignored for an existing one.
    amount_b: Option<String>,
    sender: String,
    /// How far either side may fill below the amounts. Default 100, so 1%.
    slippage_bps: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolView {
    pub txs: Vec<Tx>,
    /// The pool's address, or null when this creates it.
    pub pool: Option<String>,
    pub amount_a: String,
    pub amount_b: String,
}

#[derive(Deserialize)]
pub struct CreateRequest {
    /// The bech32 account that will own the token until it is wired.
    owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployRequest {
    hub_token: String,
    name: String,
    symbol: String,
    /// The chains to launch on; every chain with a factory when absent.
    chains: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireRequest {
    /// The token's owner, who signs, and who receives the supply.
    owner: String,
    hub_token: String,
    /// Base units.
    supply: String,
    /// The EVM account that sent the deploys, which picks its routers out of any others.
    launcher: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchView {
    pub id: String,
    /// The listing, once the hub shows the token wired.
    pub listed: Option<AssetView>,
    /// Every router any factory has made for this hub token.
    pub launches: Vec<Candidate>,
    pub refreshed_at: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeInfo {
    pub hub: String,
    pub quote_asset: String,
    pub refreshed_at: u64,
    pub chains: Vec<TradeChain>,
    pub assets: Vec<AssetView>,
}

#[derive(Debug, Serialize)]
pub struct TradeChain {
    pub name: String,
    pub kind: String,
    pub domain: u32,
    pub venue: Option<Venue>,
}

// ---------------------------------------------------------------- handlers

#[derive(Debug)]
pub struct ApiError(StatusCode, String);

fn bad(message: String) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message)
}
fn upstream(message: String) -> ApiError {
    ApiError(StatusCode::BAD_GATEWAY, message)
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for ApiError {}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

pub fn router(trade: Arc<Trade>) -> Router {
    Router::new()
        .route("/api/v1/trade", get(info))
        .route("/api/v1/trade/quote", get(quote))
        .route("/api/v1/trade/build", post(build))
        .route("/api/v1/trade/pool", post(pool))
        .route("/api/v1/trade/launch/create", post(launch_create))
        .route("/api/v1/trade/launch/deploy", post(launch_deploy))
        .route("/api/v1/trade/launch/wire", post(launch_wire))
        .route("/api/v1/trade/launch/{id}", get(launch_status))
        .with_state(trade)
}

#[derive(Deserialize)]
struct InfoQuery {
    refresh: Option<bool>,
}

async fn info(
    State(trade): State<Arc<Trade>>,
    Query(q): Query<InfoQuery>,
) -> Result<Json<TradeInfo>, ApiError> {
    if q.refresh == Some(true) {
        trade.refresh().await.map_err(|e| upstream(e.to_string()))?;
    }
    Ok(Json(trade.info().await))
}

async fn quote(
    State(trade): State<Arc<Trade>>,
    Query(q): Query<QuoteQuery>,
) -> Result<Json<QuoteView>, ApiError> {
    trade.quote(q).await.map(Json)
}

async fn build(
    State(trade): State<Arc<Trade>>,
    Json(req): Json<BuildRequest>,
) -> Result<Json<BuildView>, ApiError> {
    trade.build(req).await.map(Json)
}

async fn pool(
    State(trade): State<Arc<Trade>>,
    Json(req): Json<PoolRequest>,
) -> Result<Json<PoolView>, ApiError> {
    trade.pool(req).await.map(Json)
}

async fn launch_create(
    State(trade): State<Arc<Trade>>,
    Json(req): Json<CreateRequest>,
) -> Json<Tx> {
    Json(trade.launch_create(req))
}

async fn launch_deploy(
    State(trade): State<Arc<Trade>>,
    Json(req): Json<DeployRequest>,
) -> Result<Json<Vec<Tx>>, ApiError> {
    trade.launch_deploy(req).map(Json)
}

async fn launch_wire(
    State(trade): State<Arc<Trade>>,
    Json(req): Json<WireRequest>,
) -> Result<Json<Tx>, ApiError> {
    trade.launch_wire(req).await.map(Json)
}

async fn launch_status(
    State(trade): State<Arc<Trade>>,
    Path(id): Path<String>,
) -> Result<Json<LaunchView>, ApiError> {
    if !trade
        .catalog
        .read()
        .await
        .assets
        .contains_key(&id.to_lowercase())
    {
        // A launcher waiting on its token asks often; read the chains now rather than on the timer.
        let _ = trade.refresh().await;
    }
    trade.launch_status(&id).await.map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desk() -> Trade {
        let routers = |hub: &str| {
            ["celestia", "sepolia", "base", "eden"]
                .iter()
                .map(|c| {
                    let r = if *c == "celestia" {
                        hub.to_string()
                    } else {
                        format!("0x{}", "11".repeat(20))
                    };
                    (c.to_string(), r)
                })
                .collect()
        };
        let asset = |denom: &str| Asset {
            decimals: 6,
            denom: denom.into(),
            routers: routers("0xab"),
        };
        let venue = Venue {
            factory: format!("0x{}", "44".repeat(20)),
            positions: format!("0x{}", "55".repeat(20)),
            swap_router: format!("0x{}", "22".repeat(20)),
            quoter: format!("0x{}", "33".repeat(20)),
            fee: 3000,
            token_factory: Some(format!("0x{}", "66".repeat(20))),
            rpc: None,
        };
        let chain = |kind: &str, domain| Chain {
            kind: kind.into(),
            domain,
            rpc: None,
        };
        let config = TradeConfig {
            hub: "celestia".into(),
            hub_rest: "http://localhost:1317".into(),
            hub_mailbox: "0x68".into(),
            noop_ism: "0x01".into(),
            routing_ism: "0x02".into(),
            quote_asset: "teeUSD".into(),
            default_venue: "base".into(),
            celestia_max_fee: 1,
            router_gas: 50_000,
            assets: [
                ("TIA".into(), asset("utia")),
                ("teeUSD".into(), asset("hyperlane/x")),
            ]
            .into(),
            venues: [("sepolia".into(), venue.clone()), ("base".into(), venue)].into(),
        };
        let assets = config
            .assets
            .iter()
            .map(|(s, a)| {
                (
                    s.clone(),
                    AssetView {
                        id: s.clone(),
                        symbol: s.clone(),
                        name: s.clone(),
                        decimals: 6,
                        denom: a.denom.clone(),
                        routers: a.routers.clone(),
                        launched: false,
                        launcher: None,
                        pools: BTreeMap::new(),
                    },
                )
            })
            .collect();
        Trade {
            config,
            chains: [
                ("celestia".into(), chain("celestia", 1)),
                ("sepolia".into(), chain("ethereum", 11155111)),
                ("base".into(), chain("base", 84532)),
                ("eden".into(), chain("eden", 3735928814)),
            ]
            .into(),
            catalog: RwLock::new(Catalog {
                assets,
                ..Default::default()
            }),
            refreshing: Default::default(),
            http: reqwest::Client::new(),
        }
    }

    fn shape(steps: &[Step]) -> Vec<String> {
        steps
            .iter()
            .map(|s| match s.kind {
                StepKind::Swap => format!("swap {}>{} on {}", s.sell, s.buy, s.from),
                StepKind::Bridge => format!("{} {}>{}", s.sell, s.from, s.to),
            })
            .collect()
    }

    #[test]
    fn bridges_go_through_the_hub() {
        let t = desk();
        let tia = t.catalog.try_read().unwrap().assets["TIA"].clone();
        let mut steps = Vec::new();
        t.bridges(&mut steps, "eden", "base", &tia).unwrap();
        assert_eq!(shape(&steps), ["TIA eden>celestia", "TIA celestia>base"]);
        let mut steps = Vec::new();
        t.bridges(&mut steps, "celestia", "sepolia", &tia).unwrap();
        assert_eq!(shape(&steps), ["TIA celestia>sepolia"]);
    }

    /// Every field served is in `ui/openapi.json`, and every field there is served.
    #[test]
    fn responses_match_the_published_spec() {
        let spec: Value = serde_json::from_str(crate::api::v1::SPEC).unwrap();
        let check = |name: &str, value: Value| {
            let props = spec["components"]["schemas"][name]["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("no schema {name}"));
            let object = value.as_object().unwrap();
            let served: Vec<_> = object.keys().collect();
            let listed: Vec<_> = props.keys().collect();
            assert_eq!(
                served.len(),
                listed.len(),
                "{name}: served {served:?}, spec {listed:?}"
            );
            for key in served {
                assert!(
                    props.contains_key(key),
                    "{name}.{key} is served but not in the spec"
                );
            }
        };
        let t = desk();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let info = serde_json::to_value(rt.block_on(t.info())).unwrap();
        check("TradeInfo", info.clone());
        check("TradeAsset", info["assets"][0].clone());
        let base = info["chains"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "base")
            .unwrap();
        check("TradeChain", base.clone());
        check("Venue", base["venue"].clone());
        let step = Step::new(StepKind::Swap, "base", "base", "TIA", "teeUSD");
        check("Step", serde_json::to_value(&step).unwrap());
        check(
            "Quote",
            serde_json::to_value(QuoteView {
                from: "a".into(),
                to: "b".into(),
                sell: "TIA".into(),
                buy: "teeUSD".into(),
                amount_in: "1".into(),
                amount_out: "1".into(),
                eta_secs: 1,
                steps: vec![step],
            })
            .unwrap(),
        );
        let tx = t.evm_tx("base", "0x", vec![1], 0, "d".into()).unwrap();
        check("Tx", serde_json::to_value(&tx).unwrap());
        let ctx = t.launch_create(CreateRequest { owner: "o".into() });
        check(
            "CosmosMsg",
            serde_json::to_value(&ctx.msgs.as_ref().unwrap()[0]).unwrap(),
        );
        check(
            "Build",
            serde_json::to_value(BuildView {
                txs: vec![tx],
                amount_out: "1".into(),
                min_out: "1".into(),
            })
            .unwrap(),
        );
        check(
            "PoolResult",
            serde_json::to_value(PoolView {
                txs: vec![],
                pool: None,
                amount_a: "1".into(),
                amount_b: "1".into(),
            })
            .unwrap(),
        );
        let candidate = Candidate {
            chain: "base".into(),
            router: "0x".into(),
            launcher: "0x".into(),
            name: "n".into(),
            symbol: "s".into(),
        };
        check("Candidate", serde_json::to_value(&candidate).unwrap());
        check(
            "Launch",
            serde_json::to_value(LaunchView {
                id: "0x".into(),
                listed: None,
                launches: vec![candidate],
                refreshed_at: 1,
            })
            .unwrap(),
        );
    }

    #[test]
    fn calls_are_encoded_as_the_contracts_expect() {
        // The selectors Uniswap and Hyperlane publish.
        assert_eq!(
            hex::encode(selector("exactInput((bytes,address,uint256,uint256))")),
            "b858183f"
        );
        assert_eq!(
            hex::encode(selector("quoteExactInput(bytes,uint256)")),
            "cdca1753"
        );
        assert_eq!(
            hex::encode(selector("transferRemote(uint32,bytes32,uint256)")),
            "81b4e8b4"
        );
        assert_eq!(hex::encode(selector("multicall(bytes[])")), "ac9650d8");
        assert_eq!(
            hex::encode(selector("mint((address,address,uint24,int24,int24,uint256,uint256,uint256,uint256,address,uint256))")),
            "88316456"
        );
        assert_eq!(
            hex::encode(selector(
                "createAndInitializePoolIfNecessary(address,address,uint24,uint160)"
            )),
            "13ead562"
        );
        let account = crate::bech32::encode("celestia", &[7u8; 20]);
        assert_eq!(bytes32(&account).unwrap()[12..], [7u8; 20]);
        assert_eq!(word_u128(&uint_word(1234), 0).unwrap(), 1234);
        assert_eq!(int_word(-60)[0], 0xff);
        assert!(parse_amount("0").is_err() && parse_amount("1.5").is_err());
        assert_eq!(dynamic_bytes(b"abc").len(), 64);
    }

    #[test]
    fn the_mint_message_is_what_the_hub_decodes() {
        let token = [9u8; 32];
        let m = mint_message(1297040299, &token, &[7u8; 20], 1_000_000);
        // 77-byte header, then the 64-byte warp body.
        assert_eq!(m.len(), 77 + 64);
        assert_eq!(m[0], 3);
        assert_eq!(&m[5..9], &MINT_DOMAIN.to_be_bytes());
        assert_eq!(&m[45..77], &token);
        assert_eq!(&m[77 + 12..77 + 32], &[7u8; 20]);
        assert_eq!(
            u128::from_be_bytes(m[77 + 48..].try_into().unwrap()),
            1_000_000
        );
    }

    #[test]
    fn a_full_range_deposit_follows_the_pool_price() {
        // A price of 2 token1 per token0.
        let sqrt = (U512::from(2u8) << 192usize).root(2).to::<U256>();
        let out = amount_at_price(1_000_000, sqrt, true).unwrap();
        assert!((1_999_990..=2_000_000).contains(&out), "{out}");
        let back = amount_at_price(2_000_000, sqrt, false).unwrap();
        assert!((999_990..=1_000_010).contains(&back), "{back}");
    }
}

/// Against the live pools: `cargo test -p tee-coprocessor live_trade -- --ignored --nocapture`.
#[cfg(test)]
mod live {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn live_trade() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../deploy/coprocessor.toml.example"
        );
        let config = Config::load(path).unwrap();
        let trade = Trade::new(&config).unwrap().unwrap();
        trade.read_pools().await;
        let quote = trade
            .quote(QuoteQuery {
                from: "celestia".into(),
                sell: "TIA".into(),
                to: "arbitrum".into(),
                buy: "teeUSD".into(),
                amount: "100000000".into(),
            })
            .await
            .unwrap();
        println!("{}", serde_json::to_string_pretty(&quote).unwrap());
        let swap = quote
            .steps
            .iter()
            .find(|s| s.kind == StepKind::Swap)
            .unwrap()
            .clone();
        let built = trade
            .build(BuildRequest {
                step: swap,
                amount: None,
                sender: "0x318d22faa1e0f29eac7Ef644A8FaC676F6688d1e".into(),
                recipient: None,
                slippage_bps: None,
            })
            .await
            .unwrap();
        println!("{}", serde_json::to_string_pretty(&built).unwrap());
        let added = trade
            .pool(PoolRequest {
                chain: "arbitrum".into(),
                asset_a: "TIA".into(),
                asset_b: "teeUSD".into(),
                amount_a: "1000000".into(),
                amount_b: None,
                sender: "0x318d22faa1e0f29eac7Ef644A8FaC676F6688d1e".into(),
                slippage_bps: None,
            })
            .await
            .unwrap();
        println!("{}", serde_json::to_string_pretty(&added).unwrap());
    }
}

/// Builds against a fork with one launched token, for `devnet/scripts`-free end-to-end tests:
/// FORK_RPC, LAUNCHED (its router on arbitrum) and SENDER set; prints the JSON to send.
#[cfg(test)]
mod fork {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn fork_builds() {
        let rpc = std::env::var("FORK_RPC").unwrap();
        let launched = std::env::var("LAUNCHED").unwrap();
        let sender = std::env::var("SENDER").unwrap();
        let stage = std::env::var("STAGE").unwrap();
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../deploy/coprocessor.toml.example"
        );
        let config = Config::load(path).unwrap();
        let mut trade = Trade::new(&config).unwrap().unwrap();
        trade.chains.get_mut("arbitrum").unwrap().rpc = Some(Rpc::new(&rpc, None));
        let hub = "0x726f757465725f617070000000000000000000000000000200000000000000ff".to_string();
        trade.catalog.write().await.assets.insert(
            hub.clone(),
            AssetView {
                id: hub.clone(),
                symbol: "MOON".into(),
                name: "Moon".into(),
                decimals: 6,
                denom: format!("hyperlane/{hub}"),
                routers: [("arbitrum".to_string(), launched)].into(),
                launched: true,
                launcher: None,
                pools: BTreeMap::new(),
            },
        );
        let out = match stage.as_str() {
            "pool" => serde_json::to_value(
                trade
                    .pool(PoolRequest {
                        chain: "arbitrum".into(),
                        asset_a: "MOON".into(),
                        asset_b: "teeUSD".into(),
                        amount_a: "10000000000".into(),
                        amount_b: Some("1000000000".into()),
                        sender,
                        slippage_bps: None,
                    })
                    .await
                    .unwrap(),
            ),
            _ => {
                let mut step = Step::new(StepKind::Swap, "arbitrum", "arbitrum", "TIA", &hub);
                step.amount_in = "10000000".into();
                serde_json::to_value(
                    trade
                        .build(BuildRequest {
                            step,
                            amount: None,
                            sender,
                            recipient: None,
                            slippage_bps: None,
                        })
                        .await
                        .unwrap(),
                )
            }
        };
        println!("JSON{}", out.unwrap());
    }
}

/// The trade API alone, for working on the UI or the MCP server against the live chains:
/// `HUB_REST=http://<gateway>/rest cargo test -p tee-coprocessor serve_trade_api -- --ignored`
/// serves it on :3101 until stopped.
#[cfg(test)]
mod serve {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn serve_trade_api() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../deploy/coprocessor.toml.example"
        );
        let mut config = Config::load(path).unwrap();
        if let (Some(t), Ok(rest)) = (config.trade.as_mut(), std::env::var("HUB_REST")) {
            t.hub_rest = rest;
        }
        let trade = Arc::new(Trade::new(&config).unwrap().unwrap());
        trade.spawn_refresh();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:3101")
            .await
            .unwrap();
        axum::serve(listener, router(trade)).await.unwrap();
    }
}
