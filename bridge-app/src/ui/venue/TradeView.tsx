// Trade: any listed asset on one chain for any other on another, swapped and bridged in as
// many steps as the route needs, with the steps shown while they run.

import { useEffect, useMemo, useState } from "react";
import { CHAINS } from "../../config";
import type { ChainId } from "../../config";
import { formatAmount, NONCE_RETRYING, toBaseUnits } from "../../bridge";
import { assetBalance, fetchQuote, loadJson, runTrade, saveJson, walletsNeeded } from "../../trade";
import type { Asset, Quote, RunningTrade, Step } from "../../trade";
import { ChainPicker } from "../BridgeView";
import { describeDuration } from "../shared";
import { AssetPicker } from "./pickers";
import { createStore } from "./store";
import type { VenueProps } from "./VenuePage";

/// What a swap may fill below its quote. The API builds with the same default.
const SLIPPAGE_BPS = 50n;

const running = createStore<RunningTrade | null>(loadJson<RunningTrade>("trade"));
const notice = createStore<string | null>(null);
const setRunning = (t: RunningTrade | null) => {
  running.set(t);
  saveJson("trade", t && !t.finishedAt ? t : null);
};

export function TradeView(p: VenueProps) {
  const trade = running.use();
  const message = notice.use();
  const assets = p.info?.assets ?? [];
  const chains = useMemo(
    () => (p.info?.chains.map((c) => c.name) ?? ["celestia", "sepolia", "base", "arbitrum"]) as ChainId[],
    [p.info],
  );
  const [from, setFrom] = useState<ChainId>("celestia");
  const [sell, setSell] = useState("TIA");
  const [to, setTo] = useState<ChainId>("base");
  const [buy, setBuy] = useState("teeUSD");
  const [amount, setAmount] = useState("");
  const [quote, setQuote] = useState<Quote | null>(null);
  const [quoteError, setQuoteError] = useState<string | null>(null);
  const [sellBalance, setSellBalance] = useState<bigint | undefined>();
  const [buyBalance, setBuyBalance] = useState<bigint | undefined>();

  const symbol = (id: string) => assets.find((a) => a.id === id)?.symbol ?? id;
  const on = (chain: ChainId) => assets.filter((a) => a.routers[chain]);
  const sellAsset = assets.find((a) => a.id === sell);
  const buyAsset = assets.find((a) => a.id === buy);
  const account = (chain: ChainId) => (CHAINS[chain].kind === "evm" ? p.evm : p.cosmos);
  const sameSide = from === to && sell === buy;
  const wanted = useMemo(() => {
    try {
      return amount ? toBaseUnits(amount, symbol(sell)) : 0n;
    } catch {
      return 0n;
    }
  }, [amount, sell, assets]);

  // A chain that does not carry the picked asset falls back to the first one that does.
  useEffect(() => {
    if (assets.length && !on(from).some((a) => a.id === sell)) setSell(on(from)[0]?.id ?? "TIA");
    if (assets.length && !on(to).some((a) => a.id === buy)) setBuy(on(to)[0]?.id ?? "teeUSD");
  }, [from, to, assets]);

  // Quoted a moment after typing stops, and again whenever the route changes.
  useEffect(() => {
    setQuote(null);
    setQuoteError(null);
    if (wanted <= 0n || sameSide) return;
    let live = true;
    const timer = setTimeout(() => {
      fetchQuote(from, sell, to, buy, wanted)
        .then((q) => live && setQuote(q))
        .catch((e) => live && setQuoteError(e instanceof Error ? e.message : String(e)));
    }, 400);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [from, sell, to, buy, wanted, sameSide]);

  const finished = Boolean(trade?.finishedAt);
  useEffect(() => {
    let live = true;
    const read = (chain: ChainId, asset: Asset | undefined, set: (b: bigint | undefined) => void) => {
      const address = account(chain);
      if (!address || !asset) return set(undefined);
      assetBalance(chain, asset, address)
        .then((b) => live && set(b))
        .catch(() => live && set(undefined));
    };
    read(from, sellAsset, setSellBalance);
    read(to, buyAsset, setBuyBalance);
    return () => {
      live = false;
    };
  }, [from, to, sellAsset, buyAsset, p.evm, p.cosmos, finished]);

  const start = (t: RunningTrade) => {
    setRunning(t);
    runTrade(t, assets, { evm: p.evm, cosmos: p.cosmos }, setRunning, {
      onNonceRetry: () => notice.set(NONCE_RETRYING),
      onBridgeSent: (step, sent, messageId, originTx) => {
        notice.set(null);
        p.onTransfer({
          messageId,
          token: symbol(step.sell),
          amount: formatAmount(sent, symbol(step.sell)),
          from: step.from,
          to: step.to,
          originTx,
          reached: "dispatched",
          sentAt: Date.now(),
        });
      },
    });
  };

  if (trade) {
    return (
      <Progress
        trade={trade}
        notice={message}
        onResume={() => start(trade)}
        onDismiss={() => setRunning(null)}
      />
    );
  }

  const flip = () => {
    setFrom(to);
    setTo(from);
    setSell(buy);
    setBuy(sell);
    setAmount("");
  };

  const missing = quote ? walletsNeeded(quote).filter((w) => !(w === "MetaMask" ? p.evm : p.cosmos)) : [];
  const over = sellBalance !== undefined && wanted > sellBalance;
  const action: { label: string; run?: () => void } = !p.info
    ? { label: p.infoError ? "The trade API is not answering" : "Loading the venue…" }
    : sameSide
      ? { label: "Pick a different asset or chain" }
      : wanted <= 0n
        ? { label: "Enter an amount" }
        : quoteError
          ? { label: "No route for this trade" }
          : !quote
            ? { label: "Getting a quote…" }
            : missing.length
              ? { label: `Connect ${missing[0]}`, run: () => p.onConnect(missing[0]) }
              : over
                ? { label: `Not enough ${symbol(sell)}` }
                : {
                    label: `Trade ${amount} ${symbol(sell)} for ${symbol(buy)}`,
                    run: () =>
                      start({
                        quote,
                        symbols: Object.fromEntries(assets.map((a) => [a.id, a.symbol])),
                        steps: quote.steps.map(() => ({ status: "pending" })),
                        startedAt: Date.now(),
                      }),
                  };

  const swaps = quote?.steps.filter((s) => s.kind === "swap").length ?? 0;
  const minOut = quote ? (BigInt(quote.amountOut) * (10_000n - SLIPPAGE_BPS * BigInt(swaps))) / 10_000n : 0n;
  const walletName = (chain: ChainId) => (CHAINS[chain].kind === "evm" ? "MetaMask" : "Keplr");

  return (
    <section className="panel send venue-card">
      <div className="field">
        <div className="field-top">
          <span>You pay</span>
          <ChainPicker value={from} onChange={setFrom} chains={chains} />
        </div>
        <div className="field-row">
          <input
            className="amount"
            value={amount}
            placeholder="0"
            inputMode="decimal"
            onChange={(e) => setAmount(e.target.value.replace(/[^0-9.]/g, ""))}
            aria-label="Amount"
          />
          <AssetPicker value={sell} assets={on(from)} onChange={setSell} />
        </div>
        <div className="field-bottom">
          <span>
            {sellBalance === undefined
              ? `Connect ${walletName(from)} to see your balance`
              : `Balance ${formatAmount(sellBalance, symbol(sell))} ${symbol(sell)}`}
          </span>
          {sellBalance !== undefined && sellBalance > 0n && (
            <button className="max" onClick={() => setAmount(formatAmount(sellBalance, symbol(sell)))}>
              Max
            </button>
          )}
        </div>
      </div>

      <div className="flip-row">
        <button className="flip" onClick={flip} aria-label="Swap direction" title="Swap direction">
          <svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
            <path d="M12 5v14M6 13l6 6 6-6" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>
      </div>

      <div className="field">
        <div className="field-top">
          <span>You receive</span>
          <ChainPicker value={to} onChange={setTo} chains={chains} />
        </div>
        <div className="field-row">
          <span className={quote ? "amount received" : "amount received placeholder"}>
            {quote ? formatAmount(BigInt(quote.amountOut), symbol(buy)) : "0"}
          </span>
          <AssetPicker value={buy} assets={on(to)} onChange={setBuy} />
        </div>
        <div className="field-bottom">
          <span>
            {buyBalance === undefined
              ? `Connect ${walletName(to)} to see your balance`
              : `Balance ${formatAmount(buyBalance, symbol(buy))} ${symbol(buy)}`}
          </span>
        </div>
      </div>

      {quoteError && <p className="notice">{quoteError}</p>}

      {quote && (
        <details className="details">
          <summary>
            <span>{priceLine(quote, symbol)}</span>
            <span className="details-fee">
              {quote.steps.length} {quote.steps.length === 1 ? "step" : "steps"}, about {describeDuration(quote.etaSecs)}
            </span>
          </summary>
          <ol className="trade-steps">
            {quote.steps.map((s, i) => (
              <li key={i}>
                <span className="trade-step-dot" />
                <div>
                  <strong>{describeStep(s, symbol)}</strong>
                  <span className="muted">
                    {s.kind === "swap"
                      ? `${formatAmount(BigInt(s.amountIn), symbol(s.sell))} ${symbol(s.sell)} for about ${formatAmount(BigInt(s.amountOut), symbol(s.buy))} ${symbol(s.buy)}`
                      : `About ${describeDuration(s.etaSecs)}, verified by the enclave`}
                  </span>
                </div>
              </li>
            ))}
          </ol>
          <dl className="summary">
            <dt>You receive at least</dt>
            <dd>
              {formatAmount(minOut, symbol(buy))} {symbol(buy)}
              <span className="sub">A swap stops rather than fill more than 0.5% below its quote</span>
            </dd>
            <dt>Fees</dt>
            <dd>
              0.3% per pool, to its liquidity providers
              <span className="sub">Each bridge also charges a delivery fee in the gas token of the chain it leaves</span>
            </dd>
          </dl>
        </details>
      )}

      <button className="primary" onClick={action.run} disabled={!action.run}>
        {action.label}
      </button>
    </section>
  );
}

function Progress(p: { trade: RunningTrade; notice: string | null; onResume: () => void; onDismiss: () => void }) {
  const { quote, steps, error, finishedAt, symbols } = p.trade;
  const symbol = (id: string) => symbols[id] ?? id;
  const current = steps.findIndex((s) => s.status !== "done");
  const last = steps[steps.length - 1];
  const failedAt = steps.findIndex((s) => s.status === "failed");
  // Loaded after a reload: nothing is running it.
  const idle = !finishedAt && !error && steps.every((s) => s.status !== "active");
  const held = current > 0 ? quote.steps[current - 1] : null;

  return (
    <section className="panel send venue-card">
      <header className="panel-head">
        <h2>{finishedAt ? "Trade complete" : error ? "Trade stopped" : idle ? "Trade unfinished" : "Trading"}</h2>
        <span className="muted">
          {formatAmount(BigInt(quote.amountIn), symbol(quote.sell))} {symbol(quote.sell)} for {symbol(quote.buy)}
        </span>
      </header>

      <ol className="trade-steps live">
        {quote.steps.map((s, i) => (
          <li key={i} className={steps[i].status}>
            <span className="trade-step-dot" />
            <div>
              <strong>{describeStep(s, symbol)}</strong>
              <span className="muted">{stepDetail(s, steps[i], i === current && !error && !idle, symbol)}</span>
            </div>
          </li>
        ))}
      </ol>

      {finishedAt && last.amountOut && (
        <p className="success">
          Received {formatAmount(BigInt(last.amountOut), symbol(quote.buy))} {symbol(quote.buy)} on {CHAINS[quote.to].name}.
        </p>
      )}
      {p.notice && !finishedAt && <p className="notice">{p.notice}</p>}
      {error && (
        <p className="error">
          Step {failedAt + 1} failed: {error}
        </p>
      )}
      {(error || idle) && held && (
        <p className="notice">
          Your funds are in your own wallet as {symbol(held.buy)} on {CHAINS[held.to].name}. Continue to finish the
          trade, or move them yourself.
        </p>
      )}

      {finishedAt ? (
        <button className="primary" onClick={p.onDismiss}>
          New trade
        </button>
      ) : error || idle ? (
        <>
          <button className="primary" onClick={p.onResume}>
            Continue from step {current + 1}
          </button>
          <button className="secondary" onClick={p.onDismiss}>
            Dismiss
          </button>
        </>
      ) : (
        <button className="primary" disabled>
          Step {current + 1} of {quote.steps.length}
        </button>
      )}
    </section>
  );
}

function priceLine(q: Quote, symbol: (id: string) => string): string {
  const rate = Number((BigInt(q.amountOut) * 1_000_000n) / BigInt(q.amountIn)) / 1_000_000;
  return `1 ${symbol(q.sell)} = ${rate.toLocaleString(undefined, { maximumSignificantDigits: 5 })} ${symbol(q.buy)}`;
}

export function describeStep(s: Step, symbol: (id: string) => string): string {
  return s.kind === "swap"
    ? `Swap ${symbol(s.sell)} for ${symbol(s.buy)} on ${CHAINS[s.from].name}`
    : `Bridge ${symbol(s.sell)} from ${CHAINS[s.from].name} to ${CHAINS[s.to].name}`;
}

function stepDetail(s: Step, state: RunningTrade["steps"][number], active: boolean, symbol: (id: string) => string): string {
  if (state.status === "done" && state.amountOut) {
    const got = `${formatAmount(BigInt(state.amountOut), symbol(s.buy))} ${symbol(s.buy)}`;
    return s.kind === "swap" ? `Got ${got}` : `Delivered ${got}`;
  }
  if (state.status === "failed") return "Stopped here";
  if (!active) return s.kind === "swap" ? "Next" : `About ${describeDuration(s.etaSecs)}`;
  if (s.kind === "swap") return state.swapTx ? "Waiting for the swap to land" : "Confirm in MetaMask";
  if (state.messageId) return `Waiting for delivery, about ${describeDuration(s.etaSecs)}`;
  return `Confirm in ${CHAINS[s.from].kind === "evm" ? "MetaMask" : "Keplr"}`;
}
