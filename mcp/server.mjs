#!/usr/bin/env node
// An MCP server for the TEE Interchain Solutions venue, over stdio.
//
// Every tool calls the relayer's /api/v1/trade, which plans routes and builds transactions.
// With EVM_PRIVATE_KEY and CELESTIA_MNEMONIC set, the server also signs and sends them, and
// the long tools (trade, launch) run as jobs to poll. Without keys it still quotes, reads
// balances and returns unsigned transactions for an agent that signs elsewhere.
//
//   TEEISM_URL         the gateway, e.g. http://178.199.12.26:3000
//   EVM_PRIVATE_KEY    optional, signs on Sepolia, Base, Arbitrum and Eden
//   CELESTIA_MNEMONIC  optional, signs on the Celestia hub

import { createInterface } from "node:readline";
import { DirectSecp256k1HdWallet } from "@cosmjs/proto-signing";
import { SigningStargateClient } from "@cosmjs/stargate";
import { createPublicClient, createWalletClient, defineChain, http } from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { eventValues, registry } from "./hub.mjs";

const BASE = (process.env.TEEISM_URL ?? "http://178.199.12.26:3000").replace(/\/$/, "");
const API = `${BASE}/api`;
const evmKey = process.env.EVM_PRIVATE_KEY
  ? process.env.EVM_PRIVATE_KEY.startsWith("0x")
    ? process.env.EVM_PRIVATE_KEY
    : `0x${process.env.EVM_PRIVATE_KEY}`
  : null;
const evmAccount = evmKey ? privateKeyToAccount(evmKey) : null;
let cosmos = null;

// ---------------------------------------------------------------- the API

async function api(path, body) {
  const response = await fetch(`${API}${path}`, {
    method: body ? "POST" : "GET",
    headers: body ? { "content-type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  const json = await response.json().catch(() => null);
  if (!response.ok) throw new Error(json?.error ?? `${path} returned ${response.status}`);
  return json;
}

let infoCache = null;
async function info(refresh = false) {
  if (!infoCache || refresh || Date.now() - infoCache.at > 30_000) {
    infoCache = { at: Date.now(), value: await api(`/v1/trade${refresh ? "?refresh=true" : ""}`) };
  }
  return infoCache.value;
}

async function asset(id) {
  const all = (await info()).assets;
  const found = all.find((a) => a.id === id) ?? all.filter((a) => a.symbol === id);
  if (Array.isArray(found)) {
    if (found.length === 1) return found[0];
    throw new Error(found.length ? `${id} names more than one token; use its id` : `unknown asset ${id}`);
  }
  return found;
}

function toUnits(amount, decimals = 6) {
  const [whole, fraction = ""] = String(amount).trim().split(".");
  if (!/^\d*$/.test(whole) || !/^\d*$/.test(fraction)) throw new Error(`${amount} is not a number`);
  return BigInt(whole || "0") * 10n ** BigInt(decimals) + BigInt((fraction + "0".repeat(decimals)).slice(0, decimals) || "0");
}

function fromUnits(units, decimals = 6) {
  const n = BigInt(units);
  const unit = 10n ** BigInt(decimals);
  const fraction = (n % unit).toString().padStart(decimals, "0").replace(/0+$/, "");
  return fraction ? `${n / unit}.${fraction}` : `${n / unit}`;
}

// ---------------------------------------------------------------- chains

async function chainOf(name) {
  const c = (await info()).chains.find((x) => x.name === name);
  if (!c) throw new Error(`unknown chain ${name}`);
  return c;
}

function evmChain(c) {
  return defineChain({
    id: c.domain,
    name: c.name,
    nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
    rpcUrls: { default: { http: [`${BASE}/evm/${c.name}/`] } },
  });
}

async function hubClient() {
  if (!process.env.CELESTIA_MNEMONIC) throw new Error("set CELESTIA_MNEMONIC to sign on Celestia");
  if (!cosmos) {
    const wallet = await DirectSecp256k1HdWallet.fromMnemonic(process.env.CELESTIA_MNEMONIC, { prefix: "celestia" });
    const [account] = await wallet.getAccounts();
    const client = await SigningStargateClient.connectWithSigner(`${BASE}/rpc`, wallet, { registry });
    cosmos = { address: account.address, client };
  }
  return cosmos;
}

async function addressOn(chain) {
  const c = await chainOf(chain);
  if (c.kind === "cosmos") return (await hubClient()).address;
  if (!evmAccount) throw new Error("set EVM_PRIVATE_KEY to sign on EVM chains");
  return evmAccount.address;
}

const DISPATCH_ID = "0x788dbc1b7152732178210e7f4d9d010ef016f9eafbe66786bd7169f56e0c353a";

/// Send one built transaction and wait for it. Returns its hash and the message ids it dispatched.
async function send(tx) {
  if (tx.kind === "cosmos") {
    const { address, client } = await hubClient();
    const gas = 300_000 + 250_000 * tx.msgs.length;
    const result = await client.signAndBroadcast(address, tx.msgs, {
      amount: [{ denom: "utia", amount: String(Math.ceil(gas * 0.004)) }],
      gas: String(gas),
    });
    if (result.code !== 0) throw new Error(result.rawLog || `transaction failed (${result.code})`);
    return {
      hash: result.transactionHash,
      events: result.events,
      ids: eventValues(result.events, "EventInsertedIntoTree", "message_id"),
    };
  }
  if (!evmAccount) throw new Error("set EVM_PRIVATE_KEY to sign on EVM chains");
  const chain = evmChain(await chainOf(tx.chain));
  const wallet = createWalletClient({ account: evmAccount, chain, transport: http() });
  const reader = createPublicClient({ chain, transport: http() });
  const hash = await wallet.sendTransaction({ to: tx.to, data: tx.data, value: BigInt(tx.value ?? "0x0") });
  const receipt = await reader.waitForTransactionReceipt({ hash, timeout: 300_000 });
  if (receipt.status !== "success") throw new Error(`${hash} reverted on ${tx.chain}`);
  const ids = receipt.logs.filter((l) => l.topics[0] === DISPATCH_ID && l.topics.length === 2).map((l) => l.topics[1]);
  return { hash, events: [], ids };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/// Until the relayer reports the message delivered.
async function delivered(id) {
  for (;;) {
    const m = await api(`/v1/messages/${id}`).catch(() => null);
    if (m?.status === "delivered") return;
    await sleep(5000);
  }
}

async function balance(chain, a, owner) {
  const c = await chainOf(chain);
  if (c.kind === "cosmos") {
    const r = await fetch(`${BASE}/rest/cosmos/bank/v1beta1/balances/${owner}/by_denom?denom=${encodeURIComponent(a.denom)}`);
    return BigInt((await r.json())?.balance?.amount ?? "0");
  }
  const token = a.routers[chain];
  if (!token) return 0n;
  const reader = createPublicClient({ chain: evmChain(c), transport: http() });
  return reader.readContract({
    address: token,
    abi: [{ type: "function", name: "balanceOf", stateMutability: "view", inputs: [{ type: "address" }], outputs: [{ type: "uint256" }] }],
    functionName: "balanceOf",
    args: [owner],
  });
}

// ---------------------------------------------------------------- jobs

const jobs = new Map();
let nextJob = 1;

function job(kind, run) {
  const id = `${kind}-${nextJob++}`;
  const state = { id, kind, status: "running", log: [], startedAt: new Date().toISOString() };
  jobs.set(id, state);
  const log = (line) => state.log.push(`${new Date().toISOString().slice(11, 19)} ${line}`);
  run(log, state)
    .then((result) => Object.assign(state, { status: "done", result }))
    .catch((e) => Object.assign(state, { status: "failed", error: String(e?.message ?? e) }));
  return { job: id, poll: "call job_status with this id; trades take one to fifteen minutes" };
}

async function runTrade({ from, sell, to, buy, amount, slippage_bps }, log) {
  const [s, b] = [await asset(sell), await asset(buy)];
  const quote = await api(`/v1/trade/quote?${new URLSearchParams({ from, sell: s.id, to, buy: b.id, amount: toUnits(amount, s.decimals).toString() })}`);
  log(`quoted ${amount} ${s.symbol} for ${fromUnits(quote.amountOut, b.decimals)} ${b.symbol} in ${quote.steps.length} steps`);
  let held = BigInt(quote.amountIn);
  for (const step of quote.steps) {
    const sender = await addressOn(step.from);
    if (step.kind === "bridge") {
      const built = await api("/v1/trade/build", { step, amount: held.toString(), sender, recipient: await addressOn(step.to), slippageBps: slippage_bps });
      const sent = await send(built.txs[0]);
      log(`bridging ${step.from} to ${step.to}: ${sent.hash}, message ${sent.ids[0]}`);
      await delivered(sent.ids[0]);
      log(`delivered on ${step.to}`);
    } else {
      const bought = await asset(step.buy);
      const before = await balance(step.from, bought, sender);
      const built = await api("/v1/trade/build", { step, amount: held.toString(), sender, slippageBps: slippage_bps });
      for (const tx of built.txs) log(`${tx.description}: ${(await send(tx)).hash}`);
      let got = 0n;
      for (let i = 0; i < 20 && got <= 0n; i++) {
        got = (await balance(step.from, bought, sender)) - before;
        if (got <= 0n) await sleep(3000);
      }
      held = got;
      log(`swapped on ${step.from} for ${fromUnits(got, bought.decimals)} ${bought.symbol}`);
    }
  }
  return { received: fromUnits(held, b.decimals), asset: b.symbol, chain: to };
}

async function runLaunch({ name, symbol, supply, price, pools }, log) {
  const owner = (await hubClient()).address;
  const launcher = await addressOn("base");
  const v = await info();
  const supplyUnits = toUnits(supply);
  const priceUnits = toUnits(price);
  const plan = Object.entries(pools ?? {}).map(([chain, share]) => {
    const token = (supplyUnits * BigInt(Math.round(Number(share) * 100))) / 10_000n;
    return { chain, token, teeusd: (token * priceUnits) / 1_000_000n };
  });
  const chains = plan.map((p) => p.chain);

  const created = await send(await api("/v1/trade/launch/create", { owner }));
  const hub = eventValues(created.events, "EventCreateSyntheticToken", "token_id")[0];
  log(`created ${hub} on ${v.hub}`);
  for (const tx of await api("/v1/trade/launch/deploy", { hubToken: hub, name, symbol, chains })) {
    log(`${tx.description}: ${(await send(tx)).hash}`);
  }
  for (;;) {
    const s = await api(`/v1/trade/launch/${hub}`);
    if (chains.every((c) => s.launches.some((l) => l.chain === c && l.launcher.toLowerCase() === launcher.toLowerCase()))) break;
    await sleep(5000);
  }
  log(`wired: ${(await send(await api("/v1/trade/launch/wire", { owner, hubToken: hub, supply: supplyUnits.toString(), launcher }))).hash}`);
  while (!(await api(`/v1/trade/launch/${hub}`)).listed) await sleep(5000);
  await info(true);
  log("listed");

  const msgs = [];
  for (const p of plan) {
    const teeusd = await asset(v.quoteAsset);
    const need = p.teeusd - (await balance(p.chain, teeusd, launcher));
    for (const [id, amount] of [[hub, p.token], ...(need > 0n ? [[teeusd.id, need]] : [])]) {
      const built = await api("/v1/trade/build", {
        step: { kind: "bridge", from: v.hub, to: p.chain, sell: id, buy: id },
        amount: amount.toString(),
        sender: owner,
        recipient: launcher,
      });
      msgs.push(...built.txs[0].msgs);
    }
  }
  if (msgs.length) {
    const sent = await send({ chain: v.hub, kind: "cosmos", msgs });
    log(`bridging pool funds: ${sent.hash}`);
    for (const id of sent.ids) await delivered(id);
  }
  for (const p of plan) {
    const built = await api("/v1/trade/pool", {
      chain: p.chain,
      assetA: hub,
      assetB: v.quoteAsset,
      amountA: p.token.toString(),
      amountB: p.teeusd.toString(),
      sender: launcher,
    });
    for (const tx of built.txs) log(`${tx.description}: ${(await send(tx)).hash}`);
  }
  await info(true);
  return { id: hub, symbol, pools: chains };
}

// ---------------------------------------------------------------- tools

const TOOLS = [
  {
    name: "venue_info",
    description: "Every listed asset (TIA, teeUSD and launched tokens) with its id, the chains it is on, and its pools against teeUSD. Call first: other tools name assets by symbol or id.",
    inputSchema: { type: "object", properties: { refresh: { type: "boolean", description: "Re-read launches and pools now" } } },
    run: async ({ refresh }) => info(Boolean(refresh)),
  },
  {
    name: "quote",
    description: "Price a trade of `amount` of `sell` on chain `from` for `buy` on chain `to`, with its steps. Chains: celestia, sepolia, base, arbitrum, eden.",
    inputSchema: {
      type: "object",
      required: ["from", "sell", "to", "buy", "amount"],
      properties: {
        from: { type: "string" },
        sell: { type: "string", description: "Symbol or id" },
        to: { type: "string" },
        buy: { type: "string", description: "Symbol or id" },
        amount: { type: "string", description: "Whole units, e.g. 12.5" },
      },
    },
    run: async ({ from, sell, to, buy, amount }) => {
      const [s, b] = [await asset(sell), await asset(buy)];
      const q = await api(`/v1/trade/quote?${new URLSearchParams({ from, sell: s.id, to, buy: b.id, amount: toUnits(amount, s.decimals).toString() })}`);
      return { ...q, receive: `${fromUnits(q.amountOut, b.decimals)} ${b.symbol}`, etaMinutes: Math.ceil(q.etaSecs / 60) };
    },
  },
  {
    name: "balances",
    description: "What an address holds of every listed asset, per chain. Defaults to this server's own accounts.",
    inputSchema: { type: "object", properties: { evm: { type: "string" }, celestia: { type: "string" } } },
    run: async ({ evm, celestia }) => {
      const v = await info();
      const out = {};
      for (const c of v.chains) {
        const owner = c.kind === "evm" ? evm ?? evmAccount?.address : celestia ?? (process.env.CELESTIA_MNEMONIC ? (await hubClient()).address : null);
        if (!owner) continue;
        for (const a of v.assets.filter((x) => x.routers[c.name])) {
          const n = await balance(c.name, a, owner).catch(() => 0n);
          if (n > 0n) (out[c.name] ??= {})[a.symbol] = fromUnits(n, a.decimals);
        }
      }
      return out;
    },
  },
  {
    name: "trade",
    description: "Swap and bridge in one go with this server's keys. Starts a job; poll job_status.",
    inputSchema: {
      type: "object",
      required: ["from", "sell", "to", "buy", "amount"],
      properties: {
        from: { type: "string" },
        sell: { type: "string" },
        to: { type: "string" },
        buy: { type: "string" },
        amount: { type: "string", description: "Whole units" },
        slippage_bps: { type: "integer", description: "Default 50" },
      },
    },
    run: async (args) => job("trade", (log) => runTrade(args, log)),
  },
  {
    name: "launch",
    description: "Launch a token: fixed supply minted on Celestia, ownership renounced, bridgeable to every chain, with a pool against teeUSD on each chain named in `pools`. Needs teeUSD on Celestia for the pools. Starts a job; poll job_status.",
    inputSchema: {
      type: "object",
      required: ["name", "symbol", "supply", "price", "pools"],
      properties: {
        name: { type: "string" },
        symbol: { type: "string", description: "1 to 12 letters or digits" },
        supply: { type: "string", description: "Whole tokens, 6 decimals" },
        price: { type: "string", description: "teeUSD per token at launch" },
        pools: { type: "object", additionalProperties: { type: "number" }, description: "Chain to percent of supply for its pool, e.g. {\"base\": 5, \"arbitrum\": 5}" },
      },
    },
    run: async (args) => job("launch", (log) => runLaunch(args, log)),
  },
  {
    name: "add_liquidity",
    description: "Add full-range liquidity to the pool for two assets on one chain, creating it when absent (then amount_b sets the price). The tokens must already be on that chain.",
    inputSchema: {
      type: "object",
      required: ["chain", "asset_a", "asset_b", "amount_a"],
      properties: {
        chain: { type: "string" },
        asset_a: { type: "string" },
        asset_b: { type: "string", description: "Usually teeUSD" },
        amount_a: { type: "string", description: "Whole units" },
        amount_b: { type: "string", description: "Whole units; only for a new pool" },
      },
    },
    run: async ({ chain, asset_a, asset_b, amount_a, amount_b }) => {
      const [a, b] = [await asset(asset_a), await asset(asset_b)];
      const built = await api("/v1/trade/pool", {
        chain,
        assetA: a.id,
        assetB: b.id,
        amountA: toUnits(amount_a, a.decimals).toString(),
        amountB: amount_b ? toUnits(amount_b, b.decimals).toString() : undefined,
        sender: await addressOn(chain),
      });
      const hashes = [];
      for (const tx of built.txs) hashes.push((await send(tx)).hash);
      return { pool: built.pool ?? "created", added: { [a.symbol]: amount_a, [b.symbol]: fromUnits(built.amountB, b.decimals) }, txs: hashes };
    },
  },
  {
    name: "job_status",
    description: "Progress of a trade or launch job.",
    inputSchema: { type: "object", required: ["id"], properties: { id: { type: "string" } } },
    run: async ({ id }) => {
      const j = jobs.get(id);
      if (!j) throw new Error(`no job ${id}`);
      return j;
    },
  },
  {
    name: "build_step",
    description: "Unsigned transactions for one quote step, for an agent that signs elsewhere. Pass a step from quote, the amount actually held for it (base units), the sender, and for a bridge the recipient.",
    inputSchema: {
      type: "object",
      required: ["step", "sender"],
      properties: {
        step: { type: "object" },
        amount: { type: "string" },
        sender: { type: "string" },
        recipient: { type: "string" },
      },
    },
    run: async (args) => api("/v1/trade/build", args),
  },
];

// ---------------------------------------------------------------- MCP over stdio

const reply = (id, result) => process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);
const fail = (id, code, message) => process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id, error: { code, message } })}\n`);

const json = (v) => JSON.stringify(v, (_, x) => (typeof x === "bigint" ? x.toString() : x), 2);

createInterface({ input: process.stdin }).on("line", async (line) => {
  if (!line.trim()) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return fail(null, -32700, "parse error");
  }
  const { id, method, params } = msg;
  if (id === undefined) return; // a notification
  switch (method) {
    case "initialize":
      return reply(id, {
        protocolVersion: params?.protocolVersion ?? "2025-06-18",
        capabilities: { tools: {} },
        serverInfo: { name: "teeism", version: "0.1.0" },
        instructions:
          "A cross-chain venue over Celestia, Sepolia, Base and Arbitrum. Every transfer is verified by TEE ISMs. Start with venue_info, then quote before trade.",
      });
    case "ping":
      return reply(id, {});
    case "tools/list":
      return reply(id, { tools: TOOLS.map(({ name, description, inputSchema }) => ({ name, description, inputSchema })) });
    case "tools/call": {
      const tool = TOOLS.find((t) => t.name === params?.name);
      if (!tool) return fail(id, -32602, `unknown tool ${params?.name}`);
      try {
        return reply(id, { content: [{ type: "text", text: json(await tool.run(params.arguments ?? {})) }] });
      } catch (e) {
        return reply(id, { content: [{ type: "text", text: String(e?.message ?? e) }], isError: true });
      }
    }
    default:
      return fail(id, -32601, `no method ${method}`);
  }
});
