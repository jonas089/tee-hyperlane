// Launch & Fund: make a token that is bridgeable and tradeable on every venue, sized pool by
// pool, and add liquidity to any pool, anyone's.
//
// A launch is seven steps across both wallets. Each is recorded as it finishes, so a reload or
// a refused signature resumes where it stopped rather than starting over.

import { useEffect, useMemo, useState } from "react";
import { CHAINS } from "../../config";
import type { ChainId } from "../../config";
import { formatAmount, toBaseUnits } from "../../bridge";
import { eventValues } from "../../celestia";
import {
  accountOn,
  assetBalance,
  buildCreate,
  buildDeploy,
  buildPool,
  buildWire,
  dispatchedIds,
  fetchLaunch,
  loadJson,
  saveJson,
  sendTx,
  sleep,
  waitDelivered,
} from "../../trade";
import type { Asset, Tx, Wallets } from "../../trade";
import { ChainPicker } from "../BridgeView";
import { ChainBadge, shorten } from "../shared";
import { AssetPicker } from "./pickers";
import { createStore } from "./store";
import type { VenueProps } from "./VenuePage";
import { RELAYER_API } from "../../config";

const ZERO = "0x0000000000000000000000000000000000000000";

interface LaunchPlan {
  name: string;
  symbol: string;
  /// Base units.
  supply: string;
  /// teeUSD per token, as typed.
  price: string;
  /// Base units of the token, and of teeUSD, for each chain's pool.
  pools: Partial<Record<ChainId, { token: string; teeusd: string }>>;
  owner: string;
  launcher: string;
}

interface LaunchRun {
  plan: LaunchPlan;
  stage: number;
  hubToken?: string;
  deployed: Partial<Record<ChainId, string>>;
  funding?: { chain: ChainId; id: string }[];
  pooled: Partial<Record<ChainId, boolean>>;
  error?: string;
  active: boolean;
}

const STAGES = [
  "Create the token on Celestia",
  "Deploy it on each chain",
  "Wait for the venue to see it",
  "Mint the supply and connect the chains",
  "Wait for the listing",
  "Bridge the pool funds",
  "Create the pools",
];

const launch = createStore<LaunchRun | null>(loadJson<LaunchRun>("launch"));
const setLaunch = (r: LaunchRun | null) => {
  launch.set(r);
  saveJson("launch", r && r.stage < STAGES.length ? { ...r, active: false } : null);
};

export function LaunchView(p: VenueProps) {
  const run = launch.use();
  return (
    <div className="venue-stack">
      {run ? <LaunchProgress run={run} {...p} /> : <LaunchForm {...p} />}
      <FundCard {...p} />
    </div>
  );
}

// ---------------------------------------------------------------- the form

const DEFAULT_SHARE = 5;

function LaunchForm(p: VenueProps) {
  const venues = (p.info?.chains.filter((c) => c.venue).map((c) => c.name) ?? []) as ChainId[];
  const factories = (p.info?.chains.filter((c) => c.venue?.tokenFactory).map((c) => c.name) ?? []) as ChainId[];
  const teeusd = p.info?.assets.find((a) => a.id === p.info?.quoteAsset);
  const [name, setName] = useState("");
  const [symbol, setSymbol] = useState("");
  const [supply, setSupply] = useState("1000000000");
  const [price, setPrice] = useState("0.001");
  const [shares, setShares] = useState<Partial<Record<ChainId, number>>>({});
  const [held, setHeld] = useState<Partial<Record<ChainId, bigint>>>({});

  useEffect(() => {
    setShares((s) => (Object.keys(s).length ? s : Object.fromEntries(venues.map((c) => [c, DEFAULT_SHARE]))));
  }, [venues.join()]);

  // teeUSD wherever it already is: on Celestia it can be bridged to any pool, on a chain it
  // funds that chain's pool directly.
  useEffect(() => {
    if (!teeusd) return;
    let live = true;
    for (const chain of ["celestia", ...venues] as ChainId[]) {
      const address = CHAINS[chain].kind === "evm" ? p.evm : p.cosmos;
      if (!address) continue;
      assetBalance(chain, teeusd, address)
        .then((b) => live && setHeld((h) => ({ ...h, [chain]: b })))
        .catch(() => {});
    }
    return () => {
      live = false;
    };
  }, [teeusd?.id, p.evm, p.cosmos, venues.join()]);

  const supplyUnits = safeUnits(supply);
  const priceMicro = safeUnits(price); // teeUSD base units per whole token
  const pools = venues
    .filter((c) => (shares[c] ?? 0) > 0)
    .map((c) => {
      const token = (supplyUnits * BigInt(Math.round((shares[c] ?? 0) * 100))) / 10_000n;
      return { chain: c, token, teeusd: (token * priceMicro) / 1_000_000n };
    });
  const pooled = pools.reduce((s, x) => s + x.token, 0n);
  const kept = supplyUnits - pooled;
  // What has to come from Celestia: each chain's need less what is already there.
  const fromHub = pools.reduce((s, x) => s + max0(x.teeusd - (held[x.chain] ?? 0n)), 0n);
  const short = fromHub > (held.celestia ?? 0n);
  const fdv = (supplyUnits * priceMicro) / 1_000_000n;

  const ready = name.trim() && /^[A-Za-z0-9]{1,12}$/.test(symbol) && supplyUnits > 0n && priceMicro > 0n;
  const action: { label: string; run?: () => void } = !p.info
    ? { label: "Loading the venue…" }
    : pools.some((x) => !factories.includes(x.chain))
      ? { label: `No token factory on ${pools.filter((x) => !factories.includes(x.chain)).map((x) => CHAINS[x.chain].name).join(", ")} yet` }
      : !ready
        ? { label: "Name it, give it a symbol, a supply and a price" }
        : !p.cosmos
          ? { label: "Connect Keplr", run: () => p.onConnect("Keplr") }
          : !p.evm
            ? { label: "Connect MetaMask", run: () => p.onConnect("MetaMask") }
            : short
              ? { label: "Not enough teeUSD on Celestia for these pools" }
              : {
                  label: `Launch ${symbol}`,
                  run: () => {
                    const r: LaunchRun = {
                      plan: {
                        name: name.trim(),
                        symbol,
                        supply: supplyUnits.toString(),
                        price,
                        pools: Object.fromEntries(
                          pools.map((x) => [x.chain, { token: x.token.toString(), teeusd: x.teeusd.toString() }]),
                        ),
                        owner: p.cosmos!,
                        launcher: p.evm!,
                      },
                      stage: 0,
                      deployed: {},
                      pooled: {},
                      active: true,
                    };
                    setLaunch(r);
                    runLaunch(r, { evm: p.evm, cosmos: p.cosmos }, p);
                  },
                };

  return (
    <section className="panel venue-card">
      <header className="panel-head">
        <h2>Launch a token</h2>
        <span className="muted">Bridgeable and tradeable everywhere</span>
      </header>

      <div className="form-grid">
        <label className="input-field wide">
          <span>Name</span>
          <input value={name} maxLength={48} placeholder="Moon Coin" onChange={(e) => setName(e.target.value)} />
        </label>
        <label className="input-field">
          <span>Symbol</span>
          <input
            value={symbol}
            maxLength={12}
            placeholder="MOON"
            onChange={(e) => setSymbol(e.target.value.toUpperCase().replace(/[^A-Z0-9]/g, ""))}
          />
        </label>
        <label className="input-field">
          <span>Total supply{supplyUnits > 0n ? `, ${compact(supplyUnits)}` : ""}</span>
          <input value={supply} inputMode="numeric" onChange={(e) => setSupply(e.target.value.replace(/[^0-9]/g, ""))} />
        </label>
        <label className="input-field">
          <span>Launch price in teeUSD</span>
          <input value={price} inputMode="decimal" onChange={(e) => setPrice(e.target.value.replace(/[^0-9.]/g, ""))} />
        </label>
        <div className="input-field stat">
          <span>Fully diluted value</span>
          <strong>{pretty(fdv)} teeUSD</strong>
        </div>
      </div>
      <p className="hint">The supply is minted once on Celestia and can never grow.</p>

      <div className="alloc">
        <div className="alloc-head">
          <span>Pools</span>
          <span className="muted">Share of the supply each pool starts with</span>
        </div>
        <div className="alloc-bar" aria-hidden="true">
          {pools.map((x) => (
            <span key={x.chain} className={`seg seg-${x.chain}`} style={{ flexGrow: Number(x.token) || 0 }} />
          ))}
          <span className="seg seg-keep" style={{ flexGrow: Number(kept > 0n ? kept : 0n) }} />
        </div>
        <ul className="alloc-rows">
          {venues.map((c) => {
            const share = shares[c] ?? 0;
            const row = pools.find((x) => x.chain === c);
            return (
              <li key={c} className={share > 0 ? "" : "off"}>
                <span className={`seg-dot seg-${c}`} />
                <ChainBadge chain={c} size={26} />
                <span className="alloc-chain">{CHAINS[c].name}</span>
                <input
                  type="range"
                  min={0}
                  max={50}
                  step={0.5}
                  value={share}
                  aria-label={`${CHAINS[c].name} pool share`}
                  onChange={(e) => setShares((s) => ({ ...s, [c]: Number(e.target.value) }))}
                />
                <span className="alloc-share">{share}%</span>
                <span className="alloc-amounts">
                  {row ? (
                    <>
                      {compact(row.token)} {symbol || "tokens"}
                      <span className="muted"> + {pretty(row.teeusd)} teeUSD</span>
                    </>
                  ) : (
                    <span className="muted">No pool</span>
                  )}
                </span>
              </li>
            );
          })}
          <li className="keep">
            <span className="seg-dot seg-keep" />
            <span className="alloc-chain">You keep</span>
            <span className="alloc-amounts">
              {compact(kept > 0n ? kept : 0n)} {symbol || "tokens"} on Celestia
            </span>
          </li>
        </ul>
      </div>

      <details className="details">
        <summary>
          <span>
            Pools need {pretty(fromHub)} teeUSD from Celestia
          </span>
          <span className="details-fee">You hold {pretty(held.celestia ?? 0n)}</span>
        </summary>
        <dl className="summary">
          <dt>You pay</dt>
          <dd>
            One deployment per chain, in that chain's ETH
            <span className="sub">About 3M gas each, plus the pool transactions</span>
          </dd>
          <dt>Delivery</dt>
          <dd>
            A small fee per bridge, in TIA
            <span className="sub">The relayer pays the gas to deliver</span>
          </dd>
          <dt>Security</dt>
          <dd>
            Verified by the TEE ISMs, like TIA and teeUSD
            <span className="sub">Ownership is renounced in the same transaction that mints the supply</span>
          </dd>
        </dl>
      </details>

      <button className="primary" onClick={action.run} disabled={!action.run}>
        {action.label}
      </button>
    </section>
  );
}

// ---------------------------------------------------------------- running a launch

async function runLaunch(start: LaunchRun, wallets: Wallets, p: VenueProps) {
  let r: LaunchRun = { ...start, active: true, error: undefined };
  const save = (patch: Partial<LaunchRun>) => {
    r = { ...r, ...patch };
    setLaunch(r);
  };
  const { plan } = r;
  const chains = Object.keys(plan.pools) as ChainId[];
  save({});
  try {
    if (r.stage === 0) {
      const tx = await buildCreate(plan.owner);
      const sent = await sendTx(tx, wallets);
      const id = eventValues(sent.events, "EventCreateSyntheticToken", "token_id")[0];
      if (!id) throw new Error("the hub did not report the new token's id");
      save({ hubToken: id, stage: 1 });
    }
    const hub = r.hubToken!;
    if (r.stage === 1) {
      const txs = await buildDeploy(hub, plan.name, plan.symbol, chains);
      for (const tx of txs) {
        if (r.deployed[tx.chain]) continue;
        const sent = await sendTx(tx, wallets);
        save({ deployed: { ...r.deployed, [tx.chain]: sent.hash } });
      }
      save({ stage: 2 });
    }
    if (r.stage === 2) {
      for (;;) {
        const status = await fetchLaunch(hub);
        const mine = status.launches.filter((l) => l.launcher.toLowerCase() === plan.launcher.toLowerCase());
        if (chains.every((c) => mine.some((l) => l.chain === c))) break;
        await sleep(5000);
      }
      save({ stage: 3 });
    }
    if (r.stage === 3) {
      await sendTx(await buildWire(plan.owner, hub, plan.supply, plan.launcher), wallets);
      save({ stage: 4 });
    }
    if (r.stage === 4) {
      while (!(await fetchLaunch(hub)).listed) await sleep(5000);
      await p.reload();
      save({ stage: 5 });
    }
    if (r.stage === 5) {
      if (!r.funding) {
        const teeusd = p.info?.quoteAsset ?? "teeUSD";
        const msgs: NonNullable<Tx["msgs"]> = [];
        const order: { chain: ChainId; symbol: string; amount: bigint }[] = [];
        for (const chain of chains) {
          const pool = plan.pools[chain]!;
          const legs: [string, string, bigint][] = [[hub, plan.symbol, BigInt(pool.token)]];
          const held = await heldOn(chain, teeusd, plan.launcher);
          const need = max0(BigInt(pool.teeusd) - held);
          if (need > 0n) legs.push([teeusd, "teeUSD", need]);
          for (const [asset, symbol, amount] of legs) {
            const built = await buildBridge(chain, asset, amount, plan.owner, plan.launcher);
            msgs.push(...built.msgs!);
            order.push({ chain, symbol, amount });
          }
        }
        // Every leg in one Keplr signature.
        const tx: Tx = { chain: "celestia", kind: "cosmos", to: null, data: null, value: null, msgs, description: "Bridge the pool funds" };
        const sent = await sendTx(tx, wallets);
        const ids = await dispatchedIds(tx, sent);
        save({ funding: order.map((o, i) => ({ chain: o.chain, id: ids[i] })) });
        for (const [i, o] of order.entries()) {
          p.onTransfer({
            messageId: ids[i],
            token: o.symbol,
            amount: formatAmount(o.amount, o.symbol),
            from: "celestia",
            to: o.chain,
            originTx: sent.hash,
            reached: "dispatched",
            sentAt: Date.now(),
          });
        }
      }
      for (const f of r.funding!) await waitDelivered(f.chain, f.id);
      save({ stage: 6 });
    }
    if (r.stage === 6) {
      for (const chain of chains) {
        if (r.pooled[chain]) continue;
        const pool = plan.pools[chain]!;
        const built = await buildPool({
          chain,
          assetA: hub,
          assetB: p.info?.quoteAsset ?? "teeUSD",
          amountA: pool.token,
          amountB: pool.teeusd,
          sender: plan.launcher,
        });
        for (const tx of built.txs) await sendTx(tx, wallets);
        save({ pooled: { ...r.pooled, [chain]: true } });
      }
      save({ stage: 7, active: false });
      await p.reload();
    }
  } catch (e) {
    save({ error: e instanceof Error ? e.message : String(e), active: false });
  }
}

async function heldOn(chain: ChainId, asset: string, owner: string): Promise<bigint> {
  const info = await fetch(`${RELAYER_API}/v1/trade`).then((x) => x.json());
  const a: Asset | undefined = info.assets.find((x: Asset) => x.id === asset);
  return a ? assetBalance(chain, a, owner).catch(() => 0n) : 0n;
}

async function buildBridge(to: ChainId, asset: string, amount: bigint, sender: string, recipient: string): Promise<Tx> {
  const response = await fetch(`${RELAYER_API}/v1/trade/build`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      step: { kind: "bridge", from: "celestia", to, sell: asset, buy: asset },
      amount: amount.toString(),
      sender,
      recipient,
    }),
  });
  const body = await response.json();
  if (!response.ok) throw new Error(body?.error ?? `the trade API returned ${response.status}`);
  return body.txs[0];
}

function LaunchProgress(p: VenueProps & { run: LaunchRun }) {
  const { run } = p;
  const done = run.stage >= STAGES.length;
  const symbol = run.plan.symbol;
  const listed = p.info?.assets.find((a) => a.id === run.hubToken);
  return (
    <section className="panel venue-card">
      <header className="panel-head">
        <h2>{done ? `${symbol} is live` : run.error ? "Launch stopped" : run.active ? `Launching ${symbol}` : "Launch unfinished"}</h2>
        {run.hubToken && <code className="muted">{shorten(run.hubToken, 6)}</code>}
      </header>
      <ol className="trade-steps live">
        {STAGES.map((label, i) => {
          const status = i < run.stage ? "done" : i === run.stage ? (run.error ? "failed" : run.active ? "active" : "pending") : "pending";
          return (
            <li key={label} className={status}>
              <span className="trade-step-dot" />
              <div>
                <strong>{label}</strong>
                <span className="muted">{stageDetail(i, run, status)}</span>
              </div>
            </li>
          );
        })}
      </ol>
      {run.error && <p className="error">{run.error}</p>}
      {done && listed && (
        <p className="success">
          {symbol} trades against teeUSD on {Object.keys(listed.pools).map((c) => CHAINS[c as ChainId].name).join(", ") || "its pools"}.
          Anyone can buy it in Trade, or add to its pools below.
        </p>
      )}
      {done ? (
        <button className="primary" onClick={() => setLaunch(null)}>
          Launch another
        </button>
      ) : !run.active ? (
        <>
          <button className="primary" onClick={() => runLaunch(run, { evm: p.evm, cosmos: p.cosmos }, p)}>
            Continue from step {run.stage + 1}
          </button>
          <button className="secondary" onClick={() => setLaunch(null)}>
            Dismiss
          </button>
        </>
      ) : (
        <button className="primary" disabled>
          Step {run.stage + 1} of {STAGES.length}
        </button>
      )}
    </section>
  );
}

function stageDetail(i: number, run: LaunchRun, status: string): string {
  const chains = Object.keys(run.plan.pools).map((c) => CHAINS[c as ChainId].name);
  if (status === "failed") return "Stopped here";
  switch (i) {
    case 0:
      return status === "active" ? "Confirm in Keplr" : "One transaction";
    case 1:
      return status === "active" ? "Confirm each in MetaMask" : `${chains.join(", ")}: one transaction each`;
    case 2:
      return "Usually under a minute";
    case 3:
      return status === "active"
        ? "Confirm in Keplr"
        : `${compact(BigInt(run.plan.supply))} ${run.plan.symbol} to you; ownership renounced in the same transaction`;
    case 4:
      return "The venue checks the wiring on Celestia";
    case 5:
      return status === "active" && run.funding ? "Waiting for delivery, about two minutes" : "One Keplr transaction for every pool";
    default:
      return status === "active" ? "Confirm in MetaMask" : "Approve both tokens, then create and fill, per chain";
  }
}

// ---------------------------------------------------------------- liquidity

function FundCard(p: VenueProps) {
  const venues = (p.info?.chains.filter((c) => c.venue).map((c) => c.name) ?? []) as ChainId[];
  const [chain, setChain] = useState<ChainId>("base");
  const assets = useMemo(() => p.info?.assets.filter((a) => a.routers[chain]) ?? [], [p.info, chain]);
  const [a, setA] = useState("TIA");
  const [b, setB] = useState("teeUSD");
  const [amountA, setAmountA] = useState("");
  const [amountB, setAmountB] = useState("");
  const [preview, setPreview] = useState<{ pool: string | null; amountB: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState<string | null>(null);
  const [balances, setBalances] = useState<{ a?: bigint; b?: bigint }>({});

  const symbol = (id: string) => p.info?.assets.find((x) => x.id === id)?.symbol ?? id;
  const unitsA = safeUnits(amountA);
  const unitsB = safeUnits(amountB);

  useEffect(() => {
    if (assets.length && !assets.some((x) => x.id === a)) setA(assets[0].id);
  }, [assets]);

  // Whether the pool exists, and on an existing one, what its price asks for.
  useEffect(() => {
    setPreview(null);
    setError(null);
    if (unitsA <= 0n || a === b) return;
    let live = true;
    const timer = setTimeout(() => {
      buildPool({ chain, assetA: a, assetB: b, amountA: unitsA.toString(), amountB: unitsB > 0n ? unitsB.toString() : "1", sender: p.evm ?? ZERO })
        .then((r) => live && setPreview({ pool: r.pool, amountB: r.amountB }))
        .catch((e) => live && setError(e instanceof Error ? e.message : String(e)));
    }, 400);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [chain, a, b, unitsA, unitsB, p.evm]);

  useEffect(() => {
    if (!p.evm) return setBalances({});
    let live = true;
    const find = (id: string) => p.info?.assets.find((x) => x.id === id);
    Promise.all([find(a), find(b)].map((x) => (x ? assetBalance(chain, x, p.evm!).catch(() => undefined) : undefined)))
      .then(([ba, bb]) => live && setBalances({ a: ba, b: bb }));
    return () => {
      live = false;
    };
  }, [chain, a, b, p.evm, done]);

  const creating = preview !== null && preview.pool === null;
  const needB = preview ? (creating ? unitsB : BigInt(preview.amountB)) : 0n;
  const action: { label: string; run?: () => void } = busy
    ? { label: "Confirm in MetaMask…" }
    : a === b
      ? { label: "Pick two different tokens" }
      : unitsA <= 0n
        ? { label: "Enter an amount" }
        : creating && unitsB <= 0n
          ? { label: `Enter the ${symbol(b)} that sets the price` }
          : !p.evm
            ? { label: "Connect MetaMask", run: () => p.onConnect("MetaMask") }
            : balances.a !== undefined && unitsA > balances.a
              ? { label: `Not enough ${symbol(a)} on ${CHAINS[chain].name}` }
              : balances.b !== undefined && needB > balances.b
                ? { label: `Not enough ${symbol(b)} on ${CHAINS[chain].name}` }
                : !preview
                  ? { label: "Checking the pool…" }
                  : {
                      label: creating ? `Create the ${symbol(a)}/${symbol(b)} pool` : "Add liquidity",
                      run: async () => {
                        setBusy(true);
                        setError(null);
                        try {
                          const built = await buildPool({
                            chain,
                            assetA: a,
                            assetB: b,
                            amountA: unitsA.toString(),
                            amountB: creating ? unitsB.toString() : undefined,
                            sender: accountOn(chain, { evm: p.evm, cosmos: p.cosmos }),
                          });
                          for (const tx of built.txs) await sendTx(tx, { evm: p.evm, cosmos: p.cosmos });
                          setDone(`Added ${amountA} ${symbol(a)} and ${formatAmount(BigInt(built.amountB), symbol(b))} ${symbol(b)} on ${CHAINS[chain].name}.`);
                          setAmountA("");
                          setAmountB("");
                          await p.reload();
                        } catch (e) {
                          setError(e instanceof Error ? e.message : String(e));
                        } finally {
                          setBusy(false);
                        }
                      },
                    };

  return (
    <section className="panel venue-card">
      <header className="panel-head">
        <h2>Add liquidity</h2>
        <ChainPicker value={chain} onChange={setChain} chains={venues.length ? venues : ["base"]} />
      </header>

      <div className="field">
        <div className="field-row">
          <input className="amount" value={amountA} placeholder="0" inputMode="decimal" onChange={(e) => setAmountA(e.target.value.replace(/[^0-9.]/g, ""))} aria-label="First amount" />
          <AssetPicker value={a} assets={assets} onChange={setA} />
        </div>
        <div className="field-bottom">
          <span>{balances.a === undefined ? "Connect MetaMask to see your balance" : `Balance ${formatAmount(balances.a, symbol(a))} ${symbol(a)}`}</span>
        </div>
      </div>
      <div className="plus-row" aria-hidden="true">+</div>
      <div className="field">
        <div className="field-row">
          {creating ? (
            <input className="amount" value={amountB} placeholder="0" inputMode="decimal" onChange={(e) => setAmountB(e.target.value.replace(/[^0-9.]/g, ""))} aria-label="Second amount" />
          ) : (
            <span className={preview ? "amount received" : "amount received placeholder"}>
              {preview ? formatAmount(BigInt(preview.amountB), symbol(b)) : "0"}
            </span>
          )}
          <AssetPicker value={b} assets={assets} onChange={setB} />
        </div>
        <div className="field-bottom">
          <span>{balances.b === undefined ? "" : `Balance ${formatAmount(balances.b, symbol(b))} ${symbol(b)}`}</span>
        </div>
      </div>

      {preview && (
        <p className="hint">
          {creating
            ? `No ${symbol(a)}/${symbol(b)} pool on ${CHAINS[chain].name} yet. Your two amounts set its starting price${unitsA > 0n && unitsB > 0n ? `: 1 ${symbol(a)} = ${ratio(unitsB, unitsA)} ${symbol(b)}` : ""}.`
            : `Added at the pool's price, across the full range. You can withdraw it any time from the position in your wallet.`}
        </p>
      )}
      {error && <p className="error">{error}</p>}
      {done && <p className="success">{done}</p>}
      <p className="hint">Tokens have to be on {CHAINS[chain].name} first. Move them there in Trade.</p>

      <button className="primary" onClick={action.run} disabled={!action.run}>
        {action.label}
      </button>
    </section>
  );
}

// ---------------------------------------------------------------- numbers

function safeUnits(text: string): bigint {
  try {
    return text ? toBaseUnits(text, "") : 0n;
  } catch {
    return 0n;
  }
}

const max0 = (n: bigint) => (n > 0n ? n : 0n);

function ratio(b: bigint, a: bigint): string {
  return (Number((b * 1_000_000n) / a) / 1_000_000).toLocaleString(undefined, { maximumSignificantDigits: 6 });
}

/// 1,234,567.89 from base units.
function pretty(base: bigint): string {
  return (Number(base) / 1_000_000).toLocaleString(undefined, { maximumFractionDigits: 2 });
}

/// 1.5B, 250M, 12.3K: a supply at a glance.
function compact(base: bigint): string {
  const whole = Number(base / 1_000_000n);
  return whole.toLocaleString(undefined, { notation: "compact", maximumFractionDigits: 2 });
}
