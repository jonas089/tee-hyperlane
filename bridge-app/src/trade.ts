// The venue: trading, launching and liquidity, through the relayer's `/api/v1/trade`.
//
// The API plans and prices routes and builds every transaction; this file signs them with the
// connected wallets and waits between steps. A bridge moves an asset 1:1, so the next step gets
// exactly what was sent; a swap's output is read as the change in the bought token's balance.

import { CHAINS, RELAYER_API } from "./config";
import type { ChainId, CosmosChain, EvmChain } from "./config";
import {
  bankBalance,
  erc20Balance,
  isDelivered,
  quoteBridgeFee,
  sendEvmTx,
  waitForMessageId,
  waitForReceipt,
} from "./bridge";
import { eventValues, maxFeeFor, sendHubMsgs } from "./celestia";

export interface Venue {
  factory: string;
  positions: string;
  swapRouter: string;
  quoter: string;
  fee: number;
  tokenFactory: string | null;
}

export interface Asset {
  /// The symbol for TIA and teeUSD, the hub token id for a launched token.
  id: string;
  symbol: string;
  name: string;
  decimals: number;
  denom: string;
  routers: Partial<Record<ChainId, string>>;
  launched: boolean;
  launcher: string | null;
  /// Pools against teeUSD that hold liquidity, by chain.
  pools: Partial<Record<ChainId, string>>;
}

export interface TradeInfo {
  hub: ChainId;
  quoteAsset: string;
  refreshedAt: number;
  chains: { name: ChainId; kind: "evm" | "cosmos"; domain: number; venue: Venue | null }[];
  assets: Asset[];
}

export interface Step {
  kind: "swap" | "bridge";
  from: ChainId;
  to: ChainId;
  /// Asset ids.
  sell: string;
  buy: string;
  amountIn: string;
  amountOut: string;
  etaSecs: number;
}

export interface Quote {
  from: ChainId;
  to: ChainId;
  sell: string;
  buy: string;
  amountIn: string;
  amountOut: string;
  etaSecs: number;
  steps: Step[];
}

export interface Tx {
  chain: ChainId;
  kind: "evm" | "cosmos";
  to: string | null;
  data: string | null;
  value: string | null;
  msgs: { typeUrl: string; value: any }[] | null;
  description: string;
}

async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`${RELAYER_API}/v1/trade${path}`, init);
  const body = await response.json().catch(() => null);
  if (!response.ok) throw new Error(body?.error ?? `the trade API returned ${response.status}`);
  return body as T;
}

const post = <T>(path: string, body: unknown) =>
  api<T>(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });

export const fetchInfo = (refresh = false) => api<TradeInfo>(refresh ? "?refresh=true" : "");

export function fetchQuote(from: ChainId, sell: string, to: ChainId, buy: string, amount: bigint) {
  const q = new URLSearchParams({ from, sell, to, buy, amount: amount.toString() });
  return api<Quote>(`/quote?${q}`);
}

export const buildPool = (req: {
  chain: ChainId;
  assetA: string;
  assetB: string;
  amountA: string;
  amountB?: string;
  sender: string;
}) => post<{ txs: Tx[]; pool: string | null; amountA: string; amountB: string }>("/pool", req);

export const buildCreate = (owner: string) => post<Tx>("/launch/create", { owner });
export const buildDeploy = (hubToken: string, name: string, symbol: string, chains: ChainId[]) =>
  post<Tx[]>("/launch/deploy", { hubToken, name, symbol, chains });
export const buildWire = (owner: string, hubToken: string, supply: string, launcher: string) =>
  post<Tx>("/launch/wire", { owner, hubToken, supply, launcher });
export const fetchLaunch = (id: string) =>
  api<{ id: string; listed: Asset | null; launches: { chain: ChainId; router: string; launcher: string }[] }>(
    `/launch/${id}`,
  );

/** What `address` holds of `asset` on `chain`, in base units. */
export async function assetBalance(chain: ChainId, asset: Asset, address: string): Promise<bigint> {
  const c = CHAINS[chain];
  if (c.kind === "cosmos") return bankBalance(c, asset.denom, address);
  const token = asset.routers[chain];
  return token ? erc20Balance(c, token, address) : 0n;
}

// ---------------------------------------------------------------- sending

export interface Wallets {
  evm: string | null;
  cosmos: string | null;
}

export function accountOn(chain: ChainId, wallets: Wallets): string {
  const address = CHAINS[chain].kind === "evm" ? wallets.evm : wallets.cosmos;
  if (!address) throw new Error(`Connect ${CHAINS[chain].kind === "evm" ? "MetaMask" : "Keplr"} first`);
  return address;
}

/// Send one built transaction and wait for it. Returns the hash and, for the hub, its events.
export async function sendTx(
  tx: Tx,
  wallets: Wallets,
  onNonceRetry?: () => void,
): Promise<{ hash: string; events: readonly any[] }> {
  const sender = accountOn(tx.chain, wallets);
  const chain = CHAINS[tx.chain];
  if (tx.kind === "cosmos") {
    // A transfer offers the paymaster a ceiling over today's quote, not the API's fixed one.
    const msgs = await Promise.all(
      tx.msgs!.map(async (m) => {
        if (!m.typeUrl.endsWith("MsgRemoteTransfer")) return m;
        const to = (Object.keys(CHAINS) as ChainId[]).find((c) => CHAINS[c].domain === m.value.destination_domain)!;
        const fee = await quoteBridgeFee(tx.chain, to);
        return { ...m, value: { ...m.value, max_fee: { denom: "utia", amount: maxFeeFor(fee.amount).toString() } } };
      }),
    );
    return sendHubMsgs({ chain: chain as CosmosChain, sender, msgs });
  }
  const hash = await sendEvmTx({
    chain: chain as EvmChain,
    sender,
    to: tx.to!,
    data: tx.data!,
    value: BigInt(tx.value ?? "0x0"),
    onNonceRetry,
  });
  await waitForReceipt(chain as EvmChain, hash);
  return { hash, events: [] };
}

/// The message ids a sent bridge transaction dispatched, in order.
export async function dispatchedIds(tx: Tx, sent: { hash: string; events: readonly any[] }): Promise<string[]> {
  if (tx.kind === "cosmos") return eventValues(sent.events, "EventInsertedIntoTree", "message_id");
  return [await waitForMessageId(CHAINS[tx.chain] as EvmChain, sent.hash)];
}

export const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

export async function waitDelivered(to: ChainId, messageId: string) {
  while (!(await isDelivered(to, messageId).catch(() => false))) await sleep(5000);
}

// ---------------------------------------------------------------- running a trade

/** Where one step has got. Kept so a reload can pick the trade up where it stopped. */
export interface StepState {
  status: "pending" | "active" | "done" | "failed";
  amountIn?: string;
  amountOut?: string;
  /// A bridge's message, once dispatched, so a resumed trade waits rather than resends.
  messageId?: string;
  originTx?: string;
  /// A swap's transaction and the bought token's balance before it.
  swapTx?: string;
  balanceBefore?: string;
}

export interface RunningTrade {
  quote: Quote;
  /// Symbols by asset id, for showing the trade without the catalog.
  symbols: Record<string, string>;
  steps: StepState[];
  startedAt: number;
  finishedAt?: number;
  error?: string;
}

/// The wallets a quote's chains need, for the button to ask for.
export function walletsNeeded(quote: Quote): ("MetaMask" | "Keplr")[] {
  const out = new Set<"MetaMask" | "Keplr">();
  for (const s of quote.steps) {
    for (const c of [s.from, s.to]) out.add(CHAINS[c].kind === "evm" ? "MetaMask" : "Keplr");
  }
  return [...out];
}

/// Run the trade from its first unfinished step. `update` is called after every change.
export async function runTrade(
  trade: RunningTrade,
  assets: Asset[],
  wallets: Wallets,
  update: (t: RunningTrade) => void,
  hooks: {
    onBridgeSent?: (step: Step, amount: bigint, messageId: string, originTx: string) => void;
    onNonceRetry?: () => void;
  } = {},
): Promise<RunningTrade> {
  let t: RunningTrade = { ...trade, error: undefined, steps: trade.steps.map((s) => ({ ...s })) };
  const set = (i: number, patch: Partial<StepState>) => {
    t = { ...t, steps: t.steps.map((s, j) => (j === i ? { ...s, ...patch } : s)) };
    update(t);
  };
  const asset = (id: string) => {
    const a = assets.find((x) => x.id === id);
    if (!a) throw new Error(`${id} is not listed`);
    return a;
  };

  for (let i = 0; i < t.quote.steps.length; i++) {
    if (t.steps[i].status === "done") continue;
    const step = t.quote.steps[i];
    const amount = BigInt(i === 0 ? t.quote.amountIn : t.steps[i - 1].amountOut!);
    set(i, { status: "active", amountIn: amount.toString() });
    try {
      if (step.kind === "bridge") {
        let messageId = t.steps[i].messageId;
        if (!messageId) {
          const { txs } = await post<{ txs: Tx[] }>("/build", {
            step,
            amount: amount.toString(),
            sender: accountOn(step.from, wallets),
            recipient: accountOn(step.to, wallets),
          });
          const sent = await sendTx(txs[0], wallets, hooks.onNonceRetry);
          [messageId] = await dispatchedIds(txs[0], sent);
          set(i, { messageId, originTx: sent.hash });
          hooks.onBridgeSent?.(step, amount, messageId, sent.hash);
        }
        await waitDelivered(step.to, messageId);
        set(i, { status: "done", amountOut: amount.toString() });
      } else {
        const sender = accountOn(step.from, wallets);
        const buy = asset(step.buy);
        let { swapTx, balanceBefore } = t.steps[i];
        if (!swapTx) {
          balanceBefore = (await assetBalance(step.from, buy, sender)).toString();
          set(i, { balanceBefore });
          const { txs } = await post<{ txs: Tx[] }>("/build", { step, amount: amount.toString(), sender });
          for (const tx of txs) {
            const sent = await sendTx(tx, wallets, hooks.onNonceRetry);
            // Recorded once sent: a reload mid-swap then waits for this one rather than swapping twice.
            if (tx === txs[txs.length - 1]) set(i, { swapTx: sent.hash });
          }
        } else {
          await waitForReceipt(CHAINS[step.from] as EvmChain, swapTx);
        }
        // A public RPC can answer from a block before the swap; wait until the balance moves.
        let received = 0n;
        for (let attempt = 0; attempt < 20 && received <= 0n; attempt++) {
          received = (await assetBalance(step.from, buy, sender)) - BigInt(balanceBefore!);
          if (received <= 0n) await sleep(3000);
        }
        if (received <= 0n) throw new Error(`the swap landed but ${buy.symbol} has not moved yet; check the explorer`);
        set(i, { status: "done", amountOut: received.toString() });
      }
    } catch (e) {
      set(i, { status: "failed" });
      t = { ...t, error: e instanceof Error ? e.message : String(e) };
      update(t);
      return t;
    }
  }
  t = { ...t, finishedAt: Date.now() };
  update(t);
  return t;
}

// ---------------------------------------------------------------- persistence

const prefix = `tee-venue:${(CHAINS.celestia as CosmosChain).chainId}`;

export function loadJson<T>(key: string): T | null {
  try {
    return JSON.parse(localStorage.getItem(`${prefix}:${key}`) ?? "null");
  } catch {
    return null;
  }
}

export function saveJson(key: string, value: unknown) {
  try {
    if (value === null) localStorage.removeItem(`${prefix}:${key}`);
    else localStorage.setItem(`${prefix}:${key}`, JSON.stringify(value));
  } catch {
    // Without storage a reload forgets progress; the funds are still in the wallets.
  }
}
