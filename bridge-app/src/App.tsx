import { useCallback, useEffect, useMemo, useState } from "react";
import {
  CELESTIA_DENOM,
  CHAINS,
  ORIGIN_FINALITY,
  SLOW_ORIGIN_SECONDS,
  expectedSeconds,
  routeIsLive,
  routerFor,
} from "./config";
import type { ChainId, CosmosChain, EvmChain, TokenId } from "./config";
import {
  fetchBalance,
  fetchRouteStatus,
  formatAmount,
  messageIdFromReceipt,
  refresh,
  sendFromEvm,
  STEPS,
  toBaseUnits,
  toRecipientBytes32,
} from "./bridge";
import type { RouteStatus, Step, Transfer } from "./bridge";
import { messageIdFromCelestiaTx, sendFromCelestia } from "./celestia";
import { connectKeplr, connectMetaMask, walletFor } from "./wallets";
import type { Account } from "./wallets";

/// Every route has Celestia on one side. The bridge is a hub, not a mesh: each EVM chain's
/// ISM trusts Celestia and Celestia's trusts each EVM chain, and no EVM chain trusts another.
const COUNTERPARTIES: ChainId[] = ["sepolia", "arbitrum", "base"];
const TOKENS: TokenId[] = ["TIA", "USDC"];

const STEP_LABEL: Record<Step, string> = {
  dispatched: "Dispatched",
  attested: "Attested by enclave",
  authorised: "Authorised by ISM",
  delivered: "Delivered",
};

type Tab = "bridge" | "prover";

export default function App() {
  const [tab, setTab] = useState<Tab>("bridge");
  const [evm, setEvm] = useState<Account | null>(null);
  const [cosmos, setCosmos] = useState<Account | null>(null);
  const [counterparty, setCounterparty] = useState<ChainId>("sepolia");
  const [outbound, setOutbound] = useState(true);
  const [token, setToken] = useState<TokenId>("TIA");
  const [amount, setAmount] = useState("");
  const [recipient, setRecipient] = useState("");
  const [transfers, setTransfers] = useState<Transfer[]>(loadTransfers);
  const [balances, setBalances] = useState<Record<string, bigint>>({});
  const [routes, setRoutes] = useState<RouteStatus[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);

  useEffect(() => saveTransfers(transfers), [transfers]);

  const from: ChainId = outbound ? "celestia" : counterparty;
  const to: ChainId = outbound ? counterparty : "celestia";
  const live = routeIsLive(token, from, to);
  const source = CHAINS[from];
  const destination = CHAINS[to];

  const accountFor = useCallback(
    (chain: ChainId) => (CHAINS[chain].kind === "evm" ? evm : cosmos),
    [evm, cosmos],
  );

  const balanceKey = (chain: ChainId, t: TokenId) => `${chain}:${t}`;
  const sourceBalance = balances[balanceKey(from, token)];
  const destinationBalance = balances[balanceKey(to, token)];

  const loadBalances = useCallback(async () => {
    const wanted: [ChainId, TokenId][] = [];
    for (const chain of ["celestia", ...COUNTERPARTIES] as ChainId[]) {
      for (const t of TOKENS) wanted.push([chain, t]);
    }
    const found: Record<string, bigint> = {};
    await Promise.all(
      wanted.map(async ([chain, t]) => {
        const account = CHAINS[chain].kind === "evm" ? evm : cosmos;
        if (!account) return;
        try {
          found[balanceKey(chain, t)] = await fetchBalance(CHAINS[chain], t, account.address);
        } catch {
          // A chain whose RPC is unreachable simply has no number to show.
        }
      }),
    );
    setBalances((current) => ({ ...current, ...found }));
  }, [evm, cosmos]);

  useEffect(() => {
    loadBalances();
  }, [loadBalances]);

  const loadRoutes = useCallback(async () => {
    try {
      setRoutes(await fetchRouteStatus());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    if (tab === "prover") loadRoutes();
  }, [tab, loadRoutes]);

  const defaultRecipient = useMemo(
    () => accountFor(to)?.address ?? "",
    [accountFor, to],
  );

  const connect = useCallback(async (chainId: ChainId) => {
    setError(null);
    try {
      const chain = CHAINS[chainId];
      if (chain.kind === "evm") setEvm(await connectMetaMask(chain));
      else setCosmos(await connectKeplr(chain));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  const send = useCallback(async () => {
    setError(null);
    setSending(true);
    try {
      const target = recipient || defaultRecipient;
      if (!target) throw new Error("Enter a recipient address");

      let tx: string;
      let messageId: string;

      if (source.kind === "evm") {
        if (!evm) throw new Error("Connect MetaMask first");
        tx = await sendFromEvm({
          chain: source as EvmChain,
          token,
          destination: destination.domain,
          recipient: target,
          amount: toBaseUnits(amount, token),
          sender: evm.address,
        });
        messageId = await waitForMessageId(source as EvmChain, tx);
      } else {
        if (!cosmos) throw new Error("Connect Keplr first");
        const tokenId = routerFor(token, "celestia");
        if (!tokenId) throw new Error(`${token} is not deployed on Celestia`);
        tx = await sendFromCelestia({
          chain: source as CosmosChain,
          tokenId,
          token,
          destinationDomain: destination.domain,
          recipient: toRecipientBytes32(target),
          amount: toBaseUnits(amount, token),
          sender: cosmos.address,
        });
        messageId = await waitForCelestiaMessageId(source as CosmosChain, tx);
      }

      setTransfers((current) => [
        {
          messageId,
          token,
          amount,
          from,
          to,
          originTx: tx,
          reached: "dispatched",
          sentAt: Date.now(),
        },
        ...current,
      ]);
      setAmount("");
      loadBalances();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSending(false);
    }
  }, [
    amount, cosmos, defaultRecipient, destination, evm, from, loadBalances, recipient, source,
    to, token,
  ]);

  const update = useCallback(
    async (messageId: string) => {
      const current = transfers.find((t) => t.messageId === messageId);
      if (!current) return;
      const next = await refresh(current);
      setTransfers((all) => all.map((t) => (t.messageId === messageId ? next : t)));
      if (next.reached === "delivered") loadBalances();
    },
    [transfers, loadBalances],
  );

  return (
    <div className="app">
      <header className="topbar">
        <span className="brand">TEE Bridge</span>
        <nav className="tabs">
          <button className={tab === "bridge" ? "tab on" : "tab"} onClick={() => setTab("bridge")}>
            Bridge
          </button>
          <button className={tab === "prover" ? "tab on" : "tab"} onClick={() => setTab("prover")}>
            Prover
          </button>
        </nav>
        <div className="wallets">
          {evm ? (
            <span className="pill">{shorten(evm.address)}</span>
          ) : (
            <button className="pill action" onClick={() => connect(counterparty)}>
              Connect MetaMask
            </button>
          )}
          {cosmos ? (
            <span className="pill">{shorten(cosmos.address)}</span>
          ) : (
            <button className="pill action" onClick={() => connect("celestia")}>
              Connect Keplr
            </button>
          )}
        </div>
      </header>

      {tab === "bridge" ? (
        <main className="center">
          <section className="card">
            <div className="card-head">
              <h1>Bridge</h1>
              <select
                className="token-select"
                value={token}
                onChange={(e) => setToken(e.target.value as TokenId)}
              >
                {TOKENS.map((t) => (
                  <option key={t} value={t}>{t}</option>
                ))}
              </select>
            </div>

            <div className="field">
              <div className="field-top">
                <span>From {source.name}</span>
                <span className="balance">
                  {sourceBalance === undefined
                    ? "-"
                    : `Balance ${formatAmount(sourceBalance, token)}`}
                </span>
              </div>
              <div className="field-row">
                <input
                  className="amount"
                  value={amount}
                  placeholder="0"
                  inputMode="decimal"
                  onChange={(e) => setAmount(e.target.value)}
                />
                <span className="token-pill">{token}</span>
              </div>
              {sourceBalance !== undefined && sourceBalance > 0n && (
                <button
                  className="max"
                  onClick={() => setAmount(formatAmount(sourceBalance, token))}
                >
                  Max
                </button>
              )}
            </div>

            <button className="flip" onClick={() => setOutbound((v) => !v)} title="Swap direction">
              ↓
            </button>

            <div className="field">
              <div className="field-top">
                <span>To</span>
                <span className="balance">
                  {destinationBalance === undefined
                    ? "-"
                    : `Balance ${formatAmount(destinationBalance, token)}`}
                </span>
              </div>
              <div className="field-row">
                {to === "celestia" ? (
                  <span className="chain-fixed">{CHAINS.celestia.name}</span>
                ) : (
                  <select
                    className="chain-select"
                    value={counterparty}
                    onChange={(e) => setCounterparty(e.target.value as ChainId)}
                  >
                    {COUNTERPARTIES.map((id) => (
                      <option key={id} value={id}>{CHAINS[id].name}</option>
                    ))}
                  </select>
                )}
              </div>
            </div>

            {from !== "celestia" && (
              <div className="field">
                <div className="field-top"><span>Sending from</span></div>
                <div className="field-row">
                  <select
                    className="chain-select"
                    value={counterparty}
                    onChange={(e) => setCounterparty(e.target.value as ChainId)}
                  >
                    {COUNTERPARTIES.map((id) => (
                      <option key={id} value={id}>{CHAINS[id].name}</option>
                    ))}
                  </select>
                </div>
              </div>
            )}

            <label className="recipient">
              Recipient on {destination.name}
              <input
                value={recipient}
                placeholder={defaultRecipient || `Address on ${destination.name}`}
                onChange={(e) => setRecipient(e.target.value)}
              />
            </label>

            {ORIGIN_FINALITY[from].seconds >= SLOW_ORIGIN_SECONDS && (
              <p className="wait">
                <strong>
                  Expected {describeWhen(Date.now() + expectedSeconds(from) * 1000)}, about{" "}
                  {describeDuration(expectedSeconds(from))} from now.
                </strong>{" "}
                {ORIGIN_FINALITY[from].reason} The transfer is safe to leave. It appears below
                with its expected arrival, and you can close this page.
              </p>
            )}

            {!live && <p className="note">{token} is not deployed on this route yet.</p>}
            {error && <p className="error">{error}</p>}

            <button className="primary" onClick={send} disabled={!live || sending || !amount}>
              {sending ? "Confirm in your wallet…" : `Bridge ${token}`}
            </button>

            <p className="note">
              Signed in {walletFor(source)}. Arrival waits for {source.name} to finalise, then
              for two proofs on CPU, about {describeDuration(expectedSeconds(from))}.
            </p>
          </section>

          <section className="card list">
            <h2>Your transfers</h2>
            {transfers.length === 0 ? (
              <p className="note">Nothing sent from this browser yet.</p>
            ) : (
              <ul className="transfers">
                {transfers.map((t) => (
                  <TransferRow
                    key={t.messageId}
                    transfer={t}
                    onRefresh={() => update(t.messageId)}
                  />
                ))}
              </ul>
            )}
          </section>
        </main>
      ) : (
        <main className="center">
          <section className="card list">
            <div className="card-head">
              <h1>Prover</h1>
              <button className="pill action" onClick={loadRoutes}>Refresh</button>
            </div>
            {routes === null ? (
              <p className="note">Loading…</p>
            ) : (
              <ul className="routes">
                {routes.map((route) => (
                  <RouteCard key={route.name} route={route} />
                ))}
              </ul>
            )}
          </section>
        </main>
      )}
    </div>
  );
}

function RouteCard({ route }: { route: RouteStatus }) {
  const lastProven = route.batches[0];
  return (
    <li className="route-card">
      <div className="row">
        <strong>{route.name.replace(/-/g, " ")}</strong>
        <span className="muted">domain {route.origin} → {route.destination}</span>
      </div>
      <dl className="detail">
        <dt>Trusted origin block</dt>
        <dd>{route.height ?? "-"}</dd>
        <dt>Last proven batch</dt>
        <dd>
          {lastProven
            ? `block ${lastProven.height}, ${lastProven.messages.length} message${
                lastProven.messages.length === 1 ? "" : "s"
              }`
            : "none yet"}
        </dd>
        <dt>Proving now</dt>
        <dd>
          {route.proving
            ? `block ${route.proving.height}, ${route.proving.messages.length} message${
                route.proving.messages.length === 1 ? "" : "s"
              }`
            : "idle"}
        </dd>
        <dt>State root</dt>
        <dd><code>{route.stateRoot ? shorten(route.stateRoot, 10) : "-"}</code></dd>
      </dl>
      {route.error && <p className="error">{route.error}</p>}
    </li>
  );
}

function TransferRow({
  transfer,
  onRefresh,
}: {
  transfer: Transfer;
  onRefresh: () => void;
}) {
  const [open, setOpen] = useState(false);
  const reachedIndex = STEPS.indexOf(transfer.reached);
  const expected = transfer.sentAt + expectedSeconds(transfer.from) * 1000;
  const slow = ORIGIN_FINALITY[transfer.from].seconds >= SLOW_ORIGIN_SECONDS;

  return (
    <li>
      <div className="row">
        <div>
          <strong>{transfer.amount} {transfer.token}</strong>
          <span className="muted">
            {CHAINS[transfer.from].name} → {CHAINS[transfer.to].name}
          </span>
        </div>
        <button className="pill action" onClick={onRefresh}>Check</button>
      </div>

      <p className="timing">
        {transfer.reached === "delivered"
          ? `Landed ${describeWhen(transfer.deliveredAt ?? Date.now())}`
          : `Sent ${describeWhen(transfer.sentAt)}, expected ${describeWhen(expected)}` +
            (slow ? `, waiting on ${CHAINS[transfer.from].name}'s dispute window` : "")}
      </p>

      <ol className="steps">
        {STEPS.map((step, index) => (
          <li key={step} className={index <= reachedIndex ? "done" : ""}>
            {STEP_LABEL[step]}
          </li>
        ))}
      </ol>

      <dl className="detail">
        <dt>Message</dt>
        <dd><code>{shorten(transfer.messageId, 10)}</code></dd>
        <dt>Origin transaction</dt>
        <dd>
          <a
            href={`${(CHAINS[transfer.from] as any).explorer}/tx/${transfer.originTx}`}
            target="_blank"
            rel="noreferrer"
          >
            {shorten(transfer.originTx, 10)}
          </a>
        </dd>
      </dl>

      {transfer.failure && <p className="error">{transfer.failure}</p>}

      {transfer.attestation && (
        <div className="attestation">
          <button className="link" onClick={() => setOpen(!open)}>
            {open ? "Hide attestation" : "Show attestation"}
          </button>
          {open && (
            <dl className="detail">
              <dt>Origin block</dt>
              <dd>{transfer.attestation.height}</dd>
              <dt>State root</dt>
              <dd><code>{shorten(transfer.attestation.stateRoot, 10)}</code></dd>
              <dt>Enclave image</dt>
              <dd><code>{shorten(transfer.attestation.measurements.composeHash, 10)}</code></dd>
              <dt>OS image</dt>
              <dd><code>{shorten(transfer.attestation.measurements.osImageHash, 10)}</code></dd>
              <dt>Batch</dt>
              <dd>
                {transfer.attestation.batch.length} message
                {transfer.attestation.batch.length === 1 ? "" : "s"} attested together
              </dd>
              <dt>Quote</dt>
              <dd className="quote"><code>{transfer.attestation.quote.slice(0, 96)}…</code></dd>
            </dl>
          )}
        </div>
      )}
    </li>
  );
}

/// Times within a day are a clock reading; anything further out needs the date, or a
/// five-day wait reads as "arrives at 3pm" and looks broken.
function describeWhen(at: number): string {
  const withinADay = Math.abs(at - Date.now()) < 24 * 60 * 60 * 1000;
  return new Date(at).toLocaleString([], {
    hour: "2-digit",
    minute: "2-digit",
    ...(withinADay ? {} : { month: "short", day: "numeric" }),
  });
}

function describeDuration(seconds: number): string {
  if (seconds < 90 * 60) return `${Math.round(seconds / 60)} minutes`;
  if (seconds < 36 * 60 * 60) return `${Math.round(seconds / 3600)} hours`;
  return `${Math.round(seconds / 86400)} days`;
}

function shorten(value: string, keep = 6): string {
  if (value.length <= keep * 2 + 2) return value;
  return `${value.slice(0, keep + 2)}…${value.slice(-keep)}`;
}

async function waitForCelestiaMessageId(chain: CosmosChain, tx: string): Promise<string> {
  for (let attempt = 0; attempt < 30; attempt++) {
    const id = await messageIdFromCelestiaTx(chain, tx);
    if (id) return id;
    await new Promise((resolve) => setTimeout(resolve, 3000));
  }
  throw new Error("Transaction did not confirm in time; check the explorer");
}

async function waitForMessageId(chain: EvmChain, tx: string): Promise<string> {
  for (let attempt = 0; attempt < 40; attempt++) {
    const id = await messageIdFromReceipt(chain, tx);
    if (id) return id;
    await new Promise((resolve) => setTimeout(resolve, 3000));
  }
  throw new Error("Transaction did not confirm in time; check the explorer");
}

const STORAGE_KEY = "tee-bridge-transfers";

function loadTransfers(): Transfer[] {
  try {
    return JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "[]");
  } catch {
    return [];
  }
}

function saveTransfers(transfers: Transfer[]) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(transfers));
  } catch {
    // A browser that refuses storage still works; the list just does not survive a reload.
  }
}

export { formatAmount, CELESTIA_DENOM };
