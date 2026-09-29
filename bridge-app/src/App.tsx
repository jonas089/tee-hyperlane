import { useCallback, useEffect, useMemo, useState } from "react";
import {
  CELESTIA_DENOM,
  CHAINS,
  RELAYER_API,
  ORIGIN_FINALITY,
  SLOW_ORIGIN_SECONDS,
  expectedSeconds,
  routeIsLive,
  whyNotLive,
  routerFor,
} from "./config";
import type { ChainId, CosmosChain, EvmChain, TokenId } from "./config";
import {
  describeError,
  fetchBalance,
  formatAmount,
  formatFee,
  quoteBridgeFee,
  messageIdFromReceipt,
  refresh,
  sendFromEvm,
  STEPS,
  toBaseUnits,
  toRecipientBytes32,
} from "./bridge";
import type { BridgeFee, Step, Transfer } from "./bridge";
import type { WiredIsm } from "./ism";
import { resolveWiredIsm, shortId } from "./ism";
import { messageIdFromCelestiaTx, sendFromCelestia } from "./celestia";
import {
  connectKeplr,
  connectMetaMask,
  restoreKeplr,
  restoreMetaMask,
  walletFor,
} from "./wallets";
import type { Account } from "./wallets";

/// Every route has Celestia on one side. The bridge is a hub, not a mesh: each EVM chain's
/// ISM trusts Celestia and Celestia's trusts each EVM chain, and no EVM chain trusts another.
const COUNTERPARTIES: ChainId[] = ["sepolia", "arbitrum", "base", "eden"];
const TOKENS: TokenId[] = ["TIA", "USDC"];

/// The relayer and the gas oracle serve their own dashboards beside this one. Linking out
/// beats reimplementing them here, which is what the Prover tab was doing, worse.
const RELAYER_PORT = 3001;
const ORACLE_PORT = 3002;

/// Same host, different port, so this keeps working wherever it is deployed.
function service(port: number): string {
  return `${window.location.protocol}//${window.location.hostname}:${port}/`;
}

const STEP_LABEL: Record<Step, string> = {
  dispatched: "Dispatched",
  attested: "Attested by enclave",
  authorised: "Authorised by ISM",
  delivered: "Delivered",
};

export default function App() {
  const [evm, setEvm] = useState<Account | null>(null);
  const [cosmos, setCosmos] = useState<Account | null>(null);
  const [tab, setTab] = useState<"bridge" | "faucet" | "history">("bridge");
  const [counterparty, setCounterparty] = useState<ChainId>("sepolia");
  const [outbound, setOutbound] = useState(true);
  const [token, setToken] = useState<TokenId>("TIA");
  const [amount, setAmount] = useState("");
  const [recipient, setRecipient] = useState("");
  const [transfers, setTransfers] = useState<Transfer[]>(loadTransfers);
  const [balances, setBalances] = useState<Record<string, bigint>>({});
  const [fee, setFee] = useState<BridgeFee | null>(null);
  const [ism, setIsm] = useState<WiredIsm | null>(null);
  const [ismError, setIsmError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  // Shown once the origin transaction is in a block and the message id is known, which is the
  // moment the relayer can actually see it. Anything earlier would be claiming more than we know.
  const [confirmed, setConfirmed] = useState<Confirmation | null>(null);

  useEffect(() => saveTransfers(transfers), [transfers]);

  // Drop transfers the current deployment can never deliver.
  //
  // Redeploying an ISM starts it at the origin's head, so a message dispatched before that
  // point is below the trusted state and no batch will ever include it. Each route reports
  // the origin timestamp its ISM has reached, so "sent before that" is the exact test rather
  // than a guess about how long something ought to take.
  useEffect(() => {
    let live = true;
    fetch(`${RELAYER_API}/status`)
      .then((r) => r.json())
      .then((routes: { origin: number; destination: number; timestamp: number }[]) => {
        if (!live || !Array.isArray(routes)) return;
        setTransfers((current) =>
          current.filter((t) => {
            if (t.reached === "delivered" || t.deliveredAt) return true;
            const route = routes.find(
              (r) =>
                r.origin === CHAINS[t.from].domain && r.destination === CHAINS[t.to].domain,
            );
            return !route || t.sentAt / 1000 >= route.timestamp;
          }),
        );
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);

  // Pick up wallets this browser already authorised, so a reload does not look logged out.
  // Silent by construction: neither call prompts, and both return nothing if never connected.
  useEffect(() => {
    let current = true;
    restoreMetaMask(CHAINS.sepolia as EvmChain).then((account) => {
      if (current && account) setEvm(account);
    });
    restoreKeplr(CHAINS.celestia as CosmosChain).then((account) => {
      if (current && account) setCosmos(account);
    });
    return () => {
      current = false;
    };
  }, []);

  const from: ChainId = outbound ? "celestia" : counterparty;
  const to: ChainId = outbound ? counterparty : "celestia";
  const live = routeIsLive(token, from, to);
  const source = CHAINS[from];
  const destination = CHAINS[to];

  // Every route has Celestia on one side, so choosing a chain on one side decides the other:
  // any other chain pairs with Celestia, and Celestia pairs with whichever chain was already
  // in play. Picking the chain already on the other side swaps the two.
  const pickFrom = (chain: ChainId) => {
    if (chain === "celestia") {
      setOutbound(true);
    } else {
      setCounterparty(chain);
      setOutbound(false);
    }
  };
  const pickTo = (chain: ChainId) => {
    if (chain === "celestia") {
      setOutbound(false);
    } else {
      setCounterparty(chain);
      setOutbound(true);
    }
  };

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

  // Quoted from chain on every route change: the oracle moves it hourly, so a number baked
  // into the page would drift out of date within the hour.
  useEffect(() => {
    let current = true;
    setFee(null);
    quoteBridgeFee(from, to)
      .then((quoted) => current && setFee(quoted))
      .catch(() => current && setFee(null));
    return () => {
      current = false;
    };
  }, [from, to]);

  // Which ISM will authorise this on arrival, asked of the destination chain rather than read
  // from config, so that what is shown is what will actually be consulted.
  useEffect(() => {
    let current = true;
    setIsm(null);
    setIsmError(null);
    resolveWiredIsm(token, from, to)
      .then((wired) => current && setIsm(wired))
      .catch((e) => current && setIsmError(e instanceof Error ? e.message : String(e)));
    return () => {
      current = false;
    };
  }, [token, from, to]);



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
      setError(describeError(e));
    }
  }, []);

  const send = useCallback(async () => {
    setError(null);
    setSending(true);
    try {
      const target = recipient || defaultRecipient;
      if (!target) throw new Error("Enter a recipient address");

      // Checked here rather than left to the chain. Sending more than you hold reverts inside
      // transferRemote, and a revert reaches the browser as a provider object with the reason
      // buried in it - the user sees a failed transaction and no idea they simply overdrew.
      const wanted = toBaseUnits(amount, token);
      if (wanted <= 0n) throw new Error("Enter an amount greater than zero");
      if (sourceBalance !== undefined && wanted > sourceBalance) {
        throw new Error(
          `Not enough ${token} on ${source.name}: ` +
            `you have ${formatAmount(sourceBalance, token)} and asked to send ${amount}`,
        );
      }

      let tx: string;
      let messageId: string;

      if (source.kind === "evm") {
        if (!evm) throw new Error("Connect MetaMask first");
        tx = await sendFromEvm({
          chain: source as EvmChain,
          token,
          destination: destination.domain,
          recipient: target,
          amount: wanted,
          sender: evm.address,
        });
        messageId = await waitForMessageId(source as EvmChain, tx);
      } else {
        if (!cosmos) throw new Error("Connect Keplr first");
        const tokenId = routerFor(token, "celestia");
        if (!tokenId) throw new Error(`${token} is not deployed on Celestia`);
        // Re-quoted rather than reusing what the page showed, which may be minutes old.
        const quoted = await quoteBridgeFee(from, to);
        tx = await sendFromCelestia({
          chain: source as CosmosChain,
          tokenId,
          token,
          destinationDomain: destination.domain,
          recipient: toRecipientBytes32(target),
          amount: wanted,
          sender: cosmos.address,
          quotedFee: quoted.amount,
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
      setConfirmed({
        messageId,
        token,
        amount,
        origin: source.name,
        destination: destination.name,
        wait: describeDuration(expectedSeconds(from)),
      });
      setAmount("");
      loadBalances();
    } catch (e) {
      setError(describeError(e));
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
          <button
            className={tab === "bridge" ? "tab on" : "tab"}
            onClick={() => setTab("bridge")}
          >
            Bridge
          </button>
          <button
            className={tab === "history" ? "tab on" : "tab"}
            onClick={() => setTab("history")}
          >
            History
            {/* The count is the reason to look, so it belongs on the tab rather than behind it. */}
            {transfers.length > 0 && <span className="tab-count">{transfers.length}</span>}
          </button>
          <button
            className={tab === "faucet" ? "tab on" : "tab"}
            onClick={() => setTab("faucet")}
          >
            Faucet
          </button>
          <a className="tab" href={service(RELAYER_PORT)} target="_blank" rel="noreferrer">
            Relayer
          </a>
          <a className="tab" href={service(ORACLE_PORT)} target="_blank" rel="noreferrer">
            Gas Oracle
          </a>
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

      <main className="center">
        {tab === "faucet" ? (
          <Faucet address={cosmos?.address ?? null} onFunded={loadBalances} />
        ) : tab === "history" ? (
          <section className="card history">
            <div className="card-head">
              <h1>History</h1>
              {transfers.length > 0 && (
                <span className="history-count">
                  {transfers.length} transfer{transfers.length === 1 ? "" : "s"}
                </span>
              )}
            </div>
            {transfers.length === 0 ? (
              <p className="note">
                Nothing sent from this browser yet. Transfers are kept locally, so this list is
                per browser and not per wallet.
              </p>
            ) : (
              // Scrolls inside the card rather than the page, so the heading and the count stay
              // put while a long history moves under them.
              <div className="history-scroll">
                <ul className="transfers">
                  {transfers.map((t) => (
                    <TransferRow
                      key={t.messageId}
                      transfer={t}
                      onRefresh={() => update(t.messageId)}
                    />
                  ))}
                </ul>
              </div>
            )}
          </section>
        ) : (
          <>
          <section className="card">
            <div className="card-head">
              <h1>Bridge</h1>
            </div>

            <div className="field">
              <div className="field-top">
                <span className="panel-label">From</span>
                <ChainPicker value={from} onChange={pickFrom} />
              </div>
              <div className="field-row">
                <input
                  className="amount"
                  value={amount}
                  placeholder="0"
                  inputMode="decimal"
                  onChange={(e) => setAmount(e.target.value)}
                />
                <select
                  className="token-select"
                  value={token}
                  onChange={(e) => setToken(e.target.value as TokenId)}
                  aria-label="Token"
                >
                  {TOKENS.map((t) => (
                    <option key={t} value={t}>{t}</option>
                  ))}
                </select>
              </div>
              <div className="field-bottom">
                <span>
                  {sourceBalance === undefined
                    ? `Connect ${walletFor(source)} to see your balance`
                    : `Balance ${formatAmount(sourceBalance, token)} ${token}`}
                </span>
                {sourceBalance !== undefined && sourceBalance > 0n && (
                  <button
                    className="max"
                    onClick={() => setAmount(formatAmount(sourceBalance, token))}
                  >
                    Max
                  </button>
                )}
              </div>
            </div>

            <button
              className="flip"
              onClick={() => setOutbound((v) => !v)}
              title="Swap direction"
              aria-label="Swap direction"
            >
              ↓
            </button>

            <div className="field">
              <div className="field-top">
                <span className="panel-label">To</span>
                <ChainPicker value={to} onChange={pickTo} />
              </div>
              <div className="field-row">
                <span className={amount ? "amount received" : "amount received empty"}>
                  {amount || "0"}
                </span>
                <span className="token-pill">{token}</span>
              </div>
              <div className="field-bottom">
                <span>
                  {destinationBalance === undefined
                    ? `Connect ${walletFor(destination)} to see your balance`
                    : `Balance ${formatAmount(destinationBalance, token)} ${token}`}
                </span>
              </div>
            </div>

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

            <dl className="quote">
              <dt>Delivery fee</dt>
              <dd>{fee ? formatFee(fee) : "quoting…"}</dd>
              <dt>Arrives</dt>
              <dd>{describeDuration(expectedSeconds(from))} from now</dd>
              <dt>Verified on arrival by</dt>
              <dd>
                {ismError ? (
                  <span className="ism-bad">could not read it: {ismError}</span>
                ) : !ism ? (
                  "reading…"
                ) : (
                  <>
                    {ism.url ? (
                      <a href={ism.url} target="_blank" rel="noreferrer" title={ism.id}>
                        {shortId(ism.id)}
                      </a>
                    ) : (
                      <span title={ism.id}>{shortId(ism.id)}</span>
                    )}
                    <span className="ism-where"> on {destination.name}</span>
                    {ism.via && <span className="ism-via">{ism.via}</span>}
                  </>
                )}
              </dd>
            </dl>

            {!live && <p className="note">{whyNotLive(token, from, to)}</p>}
            {error && <p className="error">{error}</p>}

            <button className="primary" onClick={send} disabled={!live || sending || !amount}>
              {sending ? "Confirm in your wallet…" : `Bridge ${token}`}
            </button>

            <p className="note">
              Signed in {walletFor(source)}. Arrival waits for {source.name} to finalise, then the
              enclave attests it and the destination verifies the quote. About{" "}
              {describeDuration(expectedSeconds(from))}.
            </p>
          </section>

          {/* Transfers live on their own tab now. What belongs beside the form is the one
              transfer you just sent, not a growing list that pushes the form off screen. */}
          {transfers.length > 0 && (
            <p className="note recent">
              <button className="linklike" onClick={() => setTab("history")}>
                {transfers.length} transfer{transfers.length === 1 ? "" : "s"} in History
              </button>
              , most recently {describeWhen(transfers[0].sentAt)}.
            </p>
          )}
          </>
        )}
      </main>
      {confirmed && <ConfirmedDialog confirmation={confirmed} onClose={() => setConfirmed(null)} />}
    </div>
  );
}


const ALL_CHAINS: ChainId[] = ["celestia", ...COUNTERPARTIES];

/// A chain pill, as in a swap form's token picker: every chain on both sides, and the form
/// works out the other side.
function ChainPicker({ value, onChange }: { value: ChainId; onChange: (c: ChainId) => void }) {
  return (
    <select
      className="chain-select"
      value={value}
      onChange={(e) => onChange(e.target.value as ChainId)}
      aria-label="Chain"
    >
      {ALL_CHAINS.map((id) => (
        <option key={id} value={id}>{CHAINS[id].name}</option>
      ))}
    </select>
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

/// What the dialog needs to say, captured at the moment the send succeeded.
///
/// Held separately from the transfer list rather than read back out of it: the list is the
/// running record and re-renders as statuses change, and the dialog should describe the one
/// send that just happened, frozen.
type Confirmation = {
  messageId: string;
  token: TokenId;
  amount: string;
  origin: string;
  destination: string;
  wait: string;
};

/// Confirmation of a send, and an honest description of what happens next.
///
/// It says "added to the prover queue" rather than "sent", because that is the true state:
/// the origin chain has the transaction, and the relayer will pick it up, attest it, and
/// prove it. Calling it complete here is what would make the following hour feel broken.
function ConfirmedDialog({
  confirmation,
  onClose,
}: {
  confirmation: Confirmation;
  onClose: () => void;
}) {
  // Escape closes it, and so does the backdrop. A dialog with only one exit is a trap on
  // whichever device the author did not test.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="overlay" onClick={onClose} role="presentation">
      <div
        className="confirm-card"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-labelledby="confirm-title"
      >
        <svg className="tick" viewBox="0 0 52 52" aria-hidden="true">
          <circle className="tick-ring" cx="26" cy="26" r="23" />
          <path className="tick-mark" d="M15 27 l8 8 l15 -16" />
        </svg>

        <h3 id="confirm-title">Transaction confirmed</h3>
        <p className="confirm-lead">Added to the prover queue</p>

        <dl className="confirm-facts">
          <div>
            <dt>Sending</dt>
            <dd>
              {confirmation.amount} {confirmation.token}
            </dd>
          </div>
          <div>
            <dt>Route</dt>
            <dd>
              {confirmation.origin} to {confirmation.destination}
            </dd>
          </div>
          <div>
            <dt>Message</dt>
            <dd className="mono">{shorten(confirmation.messageId, 8)}</dd>
          </div>
        </dl>

        <div className="queue-track" aria-hidden="true">
          <span />
          <span />
          <span />
        </div>
        <p className="note confirm-note">
          The enclave attests it, then the destination verifies the quote. About {confirmation.wait}.
        </p>

        <button className="primary" onClick={onClose}>
          Done
        </button>
      </div>
    </div>
  );
}

const STORAGE_KEY = `tee-bridge-transfers:${(CHAINS.celestia as CosmosChain).chainId}`;

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


/// The devnet faucet: a fixed grant of TIA, once per address.
///
/// It asks the relayer API rather than signing anything here. The grant comes out of an
/// account on the deployment host, so the browser's only job is to name a recipient.
function Faucet({
  address,
  onFunded,
}: {
  address: string | null;
  onFunded: () => void;
}) {
  const [amount, setAmount] = useState<number | null>(null);
  const [enabled, setEnabled] = useState(true);
  const [claimed, setClaimed] = useState(false);
  const [busy, setBusy] = useState(false);
  const [txHash, setTxHash] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    fetch(`${RELAYER_API}/faucet`)
      .then((r) => r.json())
      .then((d) => {
        if (!live) return;
        setEnabled(Boolean(d.enabled));
        setAmount(Number(d.amountTia));
      })
      .catch(() => live && setEnabled(false));
    return () => {
      live = false;
    };
  }, []);

  // Asked per address rather than remembered in the browser: the claim is recorded on the
  // host, so clearing site data or opening another browser must not offer a second grant.
  useEffect(() => {
    setTxHash(null);
    setError(null);
    setClaimed(false);
    if (!address) return;
    let live = true;
    fetch(`${RELAYER_API}/faucet/${address}`)
      .then((r) => r.json())
      .then((d) => live && setClaimed(Boolean(d.claimed)))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [address]);

  const claim = useCallback(async () => {
    if (!address) return;
    setBusy(true);
    setError(null);
    try {
      const res = await fetch(`${RELAYER_API}/faucet`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ address }),
      });
      const body = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(body.error ?? `the faucet returned ${res.status}`);
      setClaimed(true);
      setTxHash(String(body.tx_hash));
      onFunded();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, [address, onFunded]);

  return (
    <section className="card">
      <div className="card-head">
        <h1>Faucet</h1>
      </div>

      {!enabled ? (
        <p className="note">The faucet is not configured on this deployment.</p>
      ) : (
        <>
          <p className="note">
            {amount ?? 1000} TIA on the test chain, once per address. Enough to try every
            route a few times over.
          </p>

          <div className="field">
            <div className="field-top">
              <span>Recipient</span>
            </div>
            <div className="field-row">
              <input
                className="amount"
                readOnly
                value={address ?? ""}
                placeholder="Connect Keplr to claim"
              />
            </div>
          </div>

          {!address ? (
            <p className="note">Connect Keplr and the faucet will send to that address.</p>
          ) : claimed && !txHash ? (
            <p className="note">This address has already claimed.</p>
          ) : (
            <button
              className="primary"
              disabled={busy || (claimed && !txHash)}
              onClick={claim}
            >
              {busy ? "Sending" : `Claim ${amount ?? 1000} TIA`}
            </button>
          )}

          {txHash && (
            <p className="note">
              Sent.{" "}
              <a href={`/tx/${txHash}`} target="_blank" rel="noreferrer">
                {shorten(txHash, 8)}
              </a>{" "}
              It lands in the next block.
            </p>
          )}
          {error && <p className="note error">{error}</p>}
        </>
      )}
    </section>
  );
}

export { formatAmount, CELESTIA_DENOM };
