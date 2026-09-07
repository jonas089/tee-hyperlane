import { useCallback, useEffect, useMemo, useState } from "react";
import { CHAINS, routeIsLive } from "./config";
import type { ChainId, EvmChain, TokenId } from "./config";
import {
  formatAmount,
  messageIdFromReceipt,
  refresh,
  sendFromEvm,
  STEPS,
  toBaseUnits,
} from "./bridge";
import type { Step, Transfer } from "./bridge";
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

export default function App() {
  const [evm, setEvm] = useState<Account | null>(null);
  const [cosmos, setCosmos] = useState<Account | null>(null);
  const [counterparty, setCounterparty] = useState<ChainId>("sepolia");
  const [outbound, setOutbound] = useState(true);
  const [token, setToken] = useState<TokenId>("TIA");
  const [amount, setAmount] = useState("0.1");
  const [recipient, setRecipient] = useState("");
  const [transfers, setTransfers] = useState<Transfer[]>(loadTransfers);
  const [error, setError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);

  useEffect(() => saveTransfers(transfers), [transfers]);

  const from: ChainId = outbound ? "celestia" : counterparty;
  const to: ChainId = outbound ? counterparty : "celestia";
  const live = routeIsLive(token, from, to);
  const source = CHAINS[from];
  const destination = CHAINS[to];

  const defaultRecipient = useMemo(() => {
    return destination.kind === "evm" ? evm?.address ?? "" : cosmos?.address ?? "";
  }, [destination, evm, cosmos]);

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
      if (source.kind !== "evm") {
        throw new Error(
          "Sending from Celestia needs Keplr transaction signing, which is wired on the server build",
        );
      }
      if (!evm) throw new Error("Connect MetaMask first");

      const tx = await sendFromEvm({
        chain: source as EvmChain,
        token,
        destination: destination.domain,
        recipient: target,
        amount: toBaseUnits(amount, token),
        sender: evm.address,
      });

      const messageId = await waitForMessageId(source as EvmChain, tx);
      setTransfers((current) => [
        {
          messageId,
          token,
          amount,
          from,
          to,
          originTx: tx,
          reached: "dispatched",
        },
        ...current,
      ]);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSending(false);
    }
  }, [amount, defaultRecipient, destination, evm, from, recipient, source, to, token]);

  const update = useCallback(async (messageId: string) => {
    const current = transfers.find((t) => t.messageId === messageId);
    if (!current) return;
    const next = await refresh(current);
    setTransfers((all) => all.map((t) => (t.messageId === messageId ? next : t)));
  }, [transfers]);

  return (
    <main>
      <header>
        <h1>TEE Bridge</h1>
        <p className="lede">
          TIA and USDC across Celestia and the EVM testnets. Messages are authorised by a
          light client running inside a TDX enclave, not by a validator set.
        </p>
        <div className="wallets">
          <Wallet
            label="MetaMask"
            account={evm}
            onConnect={() => connect(counterparty)}
          />
          <Wallet label="Keplr" account={cosmos} onConnect={() => connect("celestia")} />
        </div>
      </header>

      <section className="panel">
        <h2>Send</h2>
        <div className="route">
          <div className="endpoint">
            <span className="endpoint-role">From</span>
            <span className="endpoint-name">{CHAINS[from].name}</span>
          </div>
          <button className="swap" onClick={() => setOutbound((v) => !v)}>
            Swap direction
          </button>
          <div className="endpoint">
            <span className="endpoint-role">To</span>
            <span className="endpoint-name">{CHAINS[to].name}</span>
          </div>
        </div>

        <div className="grid">
          <label>
            Counterparty
            <select
              value={counterparty}
              onChange={(e) => setCounterparty(e.target.value as ChainId)}
            >
              {COUNTERPARTIES.map((id) => (
                <option key={id} value={id}>{CHAINS[id].name}</option>
              ))}
            </select>
          </label>
          <label>
            Token
            <select value={token} onChange={(e) => setToken(e.target.value as TokenId)}>
              {TOKENS.map((t) => <option key={t} value={t}>{t}</option>)}
            </select>
          </label>
          <label>
            Amount
            <input value={amount} onChange={(e) => setAmount(e.target.value)} inputMode="decimal" />
          </label>
          <label className="wide">
            Recipient on {destination.name}
            <input
              value={recipient}
              placeholder={defaultRecipient || `Address on ${destination.name}`}
              onChange={(e) => setRecipient(e.target.value)}
            />
          </label>
        </div>

        {!live && (
          <p className="note">
            {token} is not deployed on this route yet. Choose another pair.
          </p>
        )}
        {error && <p className="error">{error}</p>}

        <button onClick={send} disabled={!live || sending}>
          {sending ? "Waiting for the wallet…" : `Send ${token} to ${destination.name}`}
        </button>
        <p className="note">
          Signing happens in {walletFor(source)}. Delivery waits for the origin chain to
          finalise, then for two proofs to be produced on CPU — normally under fifteen minutes.
        </p>
      </section>

      <section className="panel">
        <h2>Transfers</h2>
        {transfers.length === 0 ? (
          <p className="note">Nothing sent from this browser yet.</p>
        ) : (
          <ul className="transfers">
            {transfers.map((t) => (
              <TransferRow key={t.messageId} transfer={t} onRefresh={() => update(t.messageId)} />
            ))}
          </ul>
        )}
      </section>
    </main>
  );
}

function Wallet({
  label,
  account,
  onConnect,
}: {
  label: string;
  account: Account | null;
  onConnect: () => void;
}) {
  if (!account) {
    return <button className="secondary" onClick={onConnect}>Connect {label}</button>;
  }
  return (
    <span className="account">
      {label} <code>{shorten(account.address)}</code>
    </span>
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

  return (
    <li>
      <div className="row">
        <div>
          <strong>{transfer.amount} {transfer.token}</strong>
          <span className="route">
            {CHAINS[transfer.from].name} → {CHAINS[transfer.to].name}
          </span>
        </div>
        <button className="secondary" onClick={onRefresh}>Check</button>
      </div>

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

function shorten(value: string, keep = 6): string {
  if (value.length <= keep * 2 + 2) return value;
  return `${value.slice(0, keep + 2)}…${value.slice(-keep)}`;
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

export { formatAmount };
