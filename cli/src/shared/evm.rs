//! EVM chains: signing, sending, deploying, and reading contracts.
//!
//! Every transaction is simulated first, so a revert is reported with its reason before any
//! gas is spent, and then sent and waited on until its receipt says it succeeded. Transactions
//! go out one at a time with the nonce tracked here, because public endpoints behind load
//! balancers hand out stale nonces and a "replacement transaction underpriced" reads like a
//! rejection rather than the collision it is.
//!
//! The endpoints are public and rate limited (Base Sepolia's allows 25 requests a second, and
//! a monthly `pccs seed` makes hundreds). Every connection here retries a 429 or a 5xx with a
//! backoff, so a limit slows a run down instead of failing it, and the receipt wait keeps
//! polling through a failed poll instead of giving up on a transaction that was already sent.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use alloy::network::TransactionBuilder;
use alloy::primitives::{address, keccak256, Address, Bytes, B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::transports::layers::RetryBackoffLayer;
use anyhow::{bail, Context, Result};

use crate::shared::chains::EvmChain;
use crate::shared::print;
use crate::shared::settings::Ctx;
use crate::shared::signer::Signer;

/// The deterministic deployment proxy every chain here has: CREATE2 with a caller-chosen salt.
pub const CREATE2_DEPLOYER: Address = address!("4e59b44847b379578588920cA78FbF26c0B4956C");

/// How long a transaction is waited on before the run reports it as sent but not landed.
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(600);
/// How often the receipt is asked for. Slow enough to stay well under a public endpoint's
/// rate limit while other requests are in flight.
const RECEIPT_POLL: Duration = Duration::from_secs(4);

/// A JSON-RPC client that retries rate limits (429) and temporary failures (5xx) with an
/// exponential backoff, honouring a `Retry-After` when the endpoint sends one. Twelve retries
/// from a second cover well over a minute of refusals before a request is given up on.
fn client(rpc: &str) -> Result<RpcClient> {
    let url = rpc.parse().with_context(|| format!("`{rpc}` is not a URL"))?;
    Ok(RpcClient::builder()
        .layer(RetryBackoffLayer::new(12, 1_000, 100))
        .http(url))
}

/// A read-only connection.
pub fn reader(rpc: &str) -> Result<DynProvider> {
    Ok(ProviderBuilder::new().connect_client(client(rpc)?).erased())
}

/// A signing connection to one chain.
pub struct Chain {
    pub chain: &'static EvmChain,
    pub rpc: String,
    pub p: DynProvider,
    pub from: Address,
    nonce: Mutex<Option<u64>>,
}

impl Chain {
    /// Signed by the contracts' owner (`Signer::owner`).
    pub async fn connect(ctx: &Ctx, chain: &'static EvmChain) -> Result<Self> {
        Self::with_signer(chain, chain.rpc(ctx), Signer::owner(ctx).await?).await
    }

    pub async fn with_signer(chain: &'static EvmChain, rpc: String, signer: Signer) -> Result<Self> {
        let from = signer.address();
        let p = ProviderBuilder::new()
            .wallet(signer.into_wallet())
            .connect_client(client(&rpc)?)
            .erased();
        let id = p
            .get_chain_id()
            .await
            .with_context(|| format!("could not reach {} at {rpc}", chain.label))?;
        if id != chain.chain_id {
            bail!(
                "{rpc} serves chain id {id}, not {} ({})",
                chain.chain_id,
                chain.label
            );
        }
        Ok(Self {
            chain,
            rpc,
            p,
            from,
            nonce: Mutex::new(None),
        })
    }

    pub fn name(&self) -> &'static str {
        self.chain.name
    }

    /// Call `to` with `data`, simulate, send, wait. `what` names the transaction in errors.
    pub async fn send(&self, what: &str, to: Address, data: Bytes) -> Result<TransactionReceipt> {
        self.send_value(what, to, data, U256::ZERO).await
    }

    pub async fn send_value(
        &self,
        what: &str,
        to: Address,
        data: Bytes,
        value: U256,
    ) -> Result<TransactionReceipt> {
        let tx = TransactionRequest::default()
            .with_from(self.from)
            .with_to(to)
            .with_input(data)
            .with_value(value);
        self.submit(what, tx).await
    }

    /// Deploy `initcode` with a plain CREATE. Returns the new contract's address, once it has
    /// code there.
    pub async fn deploy(&self, what: &str, initcode: Bytes) -> Result<Address> {
        let tx = TransactionRequest::default()
            .with_from(self.from)
            .with_deploy_code(initcode);
        let receipt = self.submit(what, tx).await?;
        let addr = receipt.contract_address.with_context(|| {
            format!(
                "{what}: receipt {} names no contract address",
                receipt.transaction_hash
            )
        })?;
        self.wait_for_code(what, addr).await?;
        Ok(addr)
    }

    /// Deploy through the CREATE2 deployer. The address is fixed by `salt` and `initcode`, so
    /// one that already has code is the same contract and is left alone.
    pub async fn create2(&self, what: &str, salt: B256, initcode: &[u8]) -> Result<Address> {
        let addr = CREATE2_DEPLOYER.create2_from_code(salt, initcode);
        if self.has_code(addr).await? {
            print::info(format!("{what}: {addr} (already deployed)"));
            return Ok(addr);
        }
        if !self.has_code(CREATE2_DEPLOYER).await? {
            bail!(
                "{} has no CREATE2 deployer at {CREATE2_DEPLOYER}",
                self.chain.label
            );
        }
        let mut data = salt.to_vec();
        data.extend_from_slice(initcode);
        self.send(what, CREATE2_DEPLOYER, data.into()).await?;
        self.wait_for_code(what, addr).await?;
        print::info(format!("{what}: {addr}"));
        Ok(addr)
    }

    /// `create2`, unless `recorded` names a contract the chain already has. A deployment is
    /// found by its record before its address is recomputed from bytecode, which need not
    /// reproduce what was deployed: a metadata hash, another compiler or another checkout
    /// gives different initcode and so a different address for the same contract.
    pub async fn create2_or(
        &self,
        what: &str,
        recorded: Option<Address>,
        salt: B256,
        initcode: &[u8],
    ) -> Result<Address> {
        if let Some(addr) = recorded {
            if self.has_code(addr).await? {
                print::info(format!("{what}: {addr} (recorded)"));
                return Ok(addr);
            }
        }
        self.create2(what, salt, initcode).await
    }

    async fn submit(&self, what: &str, tx: TransactionRequest) -> Result<TransactionReceipt> {
        let label = self.chain.label;
        // Simulate first: a revert costs nothing here and comes back with its reason.
        self.p.call(tx.clone()).await.map_err(|e| {
            anyhow::anyhow!(
                "{what} on {label} would revert: {}",
                explain_revert(&e.to_string())
            )
        })?;

        let pending = self
            .p
            .get_transaction_count(self.from)
            .pending()
            .await
            .with_context(|| format!("{what}: reading {}'s nonce on {label}", self.from))?;
        let nonce = {
            let local = *self.nonce.lock().expect("nonce lock");
            local.map_or(pending, |n| n.max(pending))
        };
        let sent = self.p.send_transaction(tx.with_nonce(nonce)).await.map_err(|e| {
            anyhow::anyhow!(
                "{what} on {label}: not accepted by {}: {}",
                self.rpc,
                explain_revert(&e.to_string())
            )
        })?;
        let hash = *sent.tx_hash();
        *self.nonce.lock().expect("nonce lock") = Some(nonce + 1);
        let receipt = self.wait_for_receipt(what, hash).await?;
        if !receipt.status() {
            bail!(
                "{what} on {label}: tx {hash} reverted in block {:?}",
                receipt.block_number
            );
        }
        Ok(receipt)
    }

    /// Poll for `hash`'s receipt until it lands or `RECEIPT_TIMEOUT` passes. A poll that fails
    /// (a rate limit the retry layer ran out of patience with, a replica that is behind) is
    /// not the transaction failing: it is remembered and polling goes on, and only a timeout
    /// reports it, with the hash, so the operator can check the transaction by hand.
    async fn wait_for_receipt(&self, what: &str, hash: B256) -> Result<TransactionReceipt> {
        let label = self.chain.label;
        let started = std::time::Instant::now();
        let mut last_error = None;
        loop {
            match self.p.get_transaction_receipt(hash).await {
                Ok(Some(receipt)) => return Ok(receipt),
                Ok(None) => {}
                Err(e) => last_error = Some(e.to_string()),
            }
            if started.elapsed() >= RECEIPT_TIMEOUT {
                let why = last_error.map_or(String::new(), |e| format!(" (last error: {e})"));
                bail!(
                    "{what} on {label}: tx {hash} sent, but no receipt within {} minutes{why}; \
                     check the transaction before sending it again",
                    RECEIPT_TIMEOUT.as_secs() / 60
                );
            }
            tokio::time::sleep(RECEIPT_POLL).await;
        }
    }

    pub async fn has_code(&self, addr: Address) -> Result<bool> {
        let code = self
            .p
            .get_code_at(addr)
            .await
            .with_context(|| format!("reading the code at {addr} on {}", self.chain.label))?;
        Ok(!code.is_empty())
    }

    /// A node behind a load balancer can answer the next read from a replica that has not seen
    /// the block yet, so a fresh deployment is given a little time to appear.
    pub async fn wait_for_code(&self, what: &str, addr: Address) -> Result<()> {
        for _ in 0..20 {
            if self.has_code(addr).await? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        bail!(
            "{what}: no code at {addr} on {} a minute after it was deployed",
            self.chain.label
        )
    }
}

/// Make a revert legible: decode `Error(string)` and name the errors an operator can act on.
pub fn explain_revert(raw: &str) -> String {
    let hints = [
        (
            "0x82b42900",
            "Unauthorized: the sending key lacks the role or ownership this needs",
        ),
        (
            "0x118cdaa7",
            "OwnableUnauthorizedAccount: the sender is not the owner",
        ),
        (
            "FORBIDDEN",
            "the storage contract does not trust this DAO: its owner must grantDao it",
        ),
        (
            "xpired",
            "an Intel artifact or its issuer is outside its validity window",
        ),
        ("insufficient funds", "the signing account cannot pay for gas"),
        (
            "nonce too low",
            "another transaction from this account landed first; re-run",
        ),
    ];
    let mut out = match revert_name(raw) {
        Some(name) => format!("{name} ({raw})"),
        None => raw.to_string(),
    };
    if let Some(i) = raw.find("0x08c379a0") {
        let hex_part: String = raw[i + 10..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if let Ok(bytes) = hex::decode(&hex_part) {
            if bytes.len() >= 64 {
                let len = U256::from_be_slice(&bytes[32..64])
                    .to::<usize>()
                    .min(bytes.len().saturating_sub(64));
                if let Ok(msg) = std::str::from_utf8(&bytes[64..64 + len]) {
                    out = format!("{msg} ({raw})");
                }
            }
        }
    }
    for (needle, hint) in hints {
        if raw.contains(needle) {
            out.push_str(&format!(" [{hint}]"));
        }
    }
    out
}

/// A compiled contract from a Foundry `out/` directory.
pub struct Artifact {
    pub bytecode: Vec<u8>,
}

impl Artifact {
    /// `out/<file>.sol/<name>.json` under `project`.
    pub fn load(project: &Path, file: &str, name: &str) -> Result<Self> {
        let path = project
            .join("out")
            .join(format!("{file}.sol"))
            .join(format!("{name}.json"));
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("no compiled {name} at {}; run `teeism build`", path.display()))?;
        let v: serde_json::Value =
            serde_json::from_str(&raw).with_context(|| format!("{} is not JSON", path.display()))?;
        let hex_code = v["bytecode"]["object"]
            .as_str()
            .with_context(|| format!("{} has no bytecode", path.display()))?;
        if hex_code.contains("__$") {
            bail!("{name} links an external library, which this deployer does not support");
        }
        let bytecode = hex::decode(hex_code.trim_start_matches("0x"))
            .with_context(|| format!("{name}'s bytecode in {} is not hex", path.display()))?;
        if bytecode.is_empty() {
            bail!(
                "{name} in {} has empty bytecode (an interface or abstract contract?)",
                path.display()
            );
        }
        Ok(Self { bytecode })
    }

    /// Creation code followed by ABI-encoded constructor arguments.
    pub fn initcode(&self, args: &[u8]) -> Vec<u8> {
        let mut code = self.bytecode.clone();
        code.extend_from_slice(args);
        code
    }
}

/// CREATE2 salt for `name` under a namespace: `keccak256("<namespace>:<name>")`, the same
/// scheme Automata's scripts use with their own prefix.
pub fn salt(namespace: &str, name: &str) -> B256 {
    keccak256(format!("{namespace}:{name}"))
}

pub fn fmt_eth(wei: U256) -> String {
    let s = alloy::primitives::utils::format_ether(wei);
    match s.find('.') {
        Some(i) if s.len() > i + 7 => s[..i + 7].to_string(),
        _ => s,
    }
}

// ---------------------------------------------------------------- naming reverts
//
// A revert carries only a 4-byte selector; the compiled ABIs under each Foundry `out/`
// directory say which error it is, so a failure reads as `Duplicate_Collateral()`, not `0x...`.

static BOOK: OnceLock<HashMap<[u8; 4], String>> = OnceLock::new();

/// Read every error out of the artifacts in these Foundry projects. Missing directories are
/// skipped: the book only ever adds names, it never stops a run.
pub fn load_revert_names(projects: &[&Path]) {
    let mut book = HashMap::new();
    for project in projects {
        let Ok(dirs) = std::fs::read_dir(project.join("out")) else {
            continue;
        };
        for dir in dirs.flatten() {
            let Ok(files) = std::fs::read_dir(dir.path()) else {
                continue;
            };
            for f in files.flatten() {
                if f.path().extension().is_some_and(|e| e == "json") {
                    add_artifact(&mut book, &f.path());
                }
            }
        }
    }
    let _ = BOOK.set(book);
}

fn add_artifact(book: &mut HashMap<[u8; 4], String>, path: &Path) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let Some(abi) = v["abi"].as_array() else { return };
    for item in abi.iter().filter(|i| i["type"] == "error") {
        let Some(name) = item["name"].as_str() else {
            continue;
        };
        let types: Vec<String> = item["inputs"]
            .as_array()
            .map(|ins| ins.iter().map(canonical_type).collect())
            .unwrap_or_default();
        let sig = format!("{name}({})", types.join(","));
        let sel = keccak256(sig.as_bytes());
        book.insert([sel[0], sel[1], sel[2], sel[3]], sig);
    }
}

fn canonical_type(input: &serde_json::Value) -> String {
    let ty = input["type"].as_str().unwrap_or("");
    match ty.strip_prefix("tuple") {
        Some(suffix) => {
            let inner: Vec<String> = input["components"]
                .as_array()
                .map(|c| c.iter().map(canonical_type).collect())
                .unwrap_or_default();
            format!("({}){suffix}", inner.join(","))
        }
        None => ty.to_string(),
    }
}

/// The name of the first known error selector in `text` (an RPC error message carrying
/// revert data), if any.
fn revert_name(text: &str) -> Option<String> {
    let book = BOOK.get()?;
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(off) = text[i..].find("0x") {
        let start = i + off + 2;
        let hex_len = bytes[start..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count();
        if hex_len >= 8 {
            if let Ok(sel) = hex::decode(&text[start..start + 8]) {
                if let Some(name) = book.get(&[sel[0], sel[1], sel[2], sel[3]]) {
                    return Some(name.clone());
                }
            }
        }
        i = start + hex_len.max(1);
        if i >= text.len() {
            break;
        }
    }
    None
}
