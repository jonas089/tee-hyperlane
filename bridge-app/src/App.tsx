import { useCallback, useEffect, useMemo, useState } from "react";
import {
  CELESTIA_DENOM,
  CHAINS,
  RELAYER_API,
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
  quoteBridgeFee,
  messageIdFromReceipt,
  refresh,
  sendFromEvm,
  toBaseUnits,
  toRecipientBytes32,
} from "./bridge";
import type { BridgeFee, Transfer } from "./bridge";
import type { WiredIsm } from "./ism";
import { resolveWiredIsm } from "./ism";
import { messageIdFromCelestiaTx, sendFromCelestia } from "./celestia";
import {
  connectKeplr,
  connectMetaMask,
  restoreKeplr,
  restoreMetaMask,
} from "./wallets";
import type { Account } from "./wallets";
import { BridgeView } from "./ui/BridgeView";
import { FaucetView } from "./ui/FaucetView";
import { HistoryView } from "./ui/HistoryView";
import { describeDuration, shorten } from "./ui/shared";

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

type Tab = "bridge" | "faucet" | "history";

function tabFromHash(): Tab {
  const name = window.location.hash.replace(/^#\/?/, "").split("?")[0];
  return name === "history" || name === "faucet" ? name : "bridge";
}

/// The search a `#history?q=…` link carries.
function queryFromHash(): string {
  const [, query = ""] = window.location.hash.split("?");
  return new URLSearchParams(query).get("q") ?? "";
}

export default function App() {
  const [evm, setEvm] = useState<Account | null>(null);
  const [cosmos, setCosmos] = useState<Account | null>(null);
  // The tab lives in the URL hash, so a reload keeps it and `#history?q=<tx>` links a search.
  const [tab, setTabState] = useState<Tab>(tabFromHash);
  const setTab = useCallback((next: Tab) => {
    setTabState(next);
    if (tabFromHash() !== next) window.location.hash = next === "bridge" ? "" : next;
  }, []);
  useEffect(() => {
    const onHash = () => setTabState(tabFromHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);
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

  const tabs: { id: typeof tab; label: string }[] = [
    { id: "bridge", label: "Bridge" },
    { id: "history", label: "History" },
    { id: "faucet", label: "Faucet" },
  ];

  return (
    <div className="app">
      <header className="topbar">
        <div className="topbar-inner">
          <a className="brand" href="#" onClick={(e) => { e.preventDefault(); setTab("bridge"); }}>
            <span>TEE Interchain Solutions</span>
          </a>
          <nav className="tabs">
            {tabs.map((t) => (
              <button key={t.id} className={tab === t.id ? "tab on" : "tab"} onClick={() => setTab(t.id)}>
                {t.label}
                {t.id === "history" && transfers.length > 0 && <span className="tab-count">{transfers.length}</span>}
              </button>
            ))}
            <a className="tab ext" href={service(RELAYER_PORT)} target="_blank" rel="noreferrer">
              Relayer ↗
            </a>
            <a className="tab ext" href={service(ORACLE_PORT)} target="_blank" rel="noreferrer">
              Gas Oracle ↗
            </a>
          </nav>
          <div className="wallets">
            <WalletButton label="MetaMask" account={evm} onConnect={() => connect(counterparty)} />
            <WalletButton label="Keplr" account={cosmos} onConnect={() => connect("celestia")} />
          </div>
        </div>
      </header>

      <main className={tab === "history" ? "page" : "page bridge-page"}>
        {tab === "bridge" ? (
          <BridgeView
            from={from}
            to={to}
            token={token}
            tokens={TOKENS}
            amount={amount}
            recipient={recipient}
            defaultRecipient={defaultRecipient}
            sourceBalance={sourceBalance}
            destinationBalance={destinationBalance}
            fee={fee}
            ism={ism}
            ismError={ismError}
            live={live}
            notLive={whyNotLive(token, from, to)}
            error={error}
            sending={sending}
            connected={Boolean(accountFor(from))}
            onConnect={() => connect(from)}
            onPickFrom={pickFrom}
            onPickTo={pickTo}
            onFlip={() => setOutbound((v) => !v)}
            onToken={setToken}
            onAmount={setAmount}
            onRecipient={setRecipient}
            onSend={send}
          />
        ) : tab === "history" ? (
          <HistoryView
            transfers={transfers}
            onRefresh={update}
            relayerUrl={service(RELAYER_PORT)}
            initialQuery={queryFromHash()}
          />
        ) : (
          <FaucetView
            address={cosmos?.address ?? null}
            onFunded={loadBalances}
            onConnect={() => connect("celestia")}
          />
        )}
      </main>

      {confirmed && <ConfirmedDialog confirmation={confirmed} onClose={() => setConfirmed(null)} />}
    </div>
  );
}

function WalletButton({
  label,
  account,
  onConnect,
}: {
  label: string;
  account: Account | null;
  onConnect: () => void;
}) {
  return account ? (
    <span className="wallet on" title={account.address}>
      <span className="wallet-dot" />
      {shorten(account.address, 4)}
    </span>
  ) : (
    <button className="wallet" onClick={onConnect}>
      Connect {label}
    </button>
  );
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
/// It says the transfer is on its way rather than done, because that is the true state: the
/// origin chain has the transaction, and the relayer still has to have it attested and
/// delivered.
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
        <p className="confirm-lead">On its way to the enclave</p>

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


export { formatAmount, CELESTIA_DENOM };
