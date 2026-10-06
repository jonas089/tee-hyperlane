// The send form, and a short summary of the route beside it.

import { CHAINS, ORIGIN_FINALITY, SLOW_ORIGIN_SECONDS, expectedSeconds } from "../config";
import type { ChainId, TokenId } from "../config";
import { formatAmount, formatFee } from "../bridge";
import type { BridgeFee } from "../bridge";
import type { WiredIsm } from "../ism";
import { shortId } from "../ism";
import { walletFor } from "../wallets";
import { ALL_CHAINS, ChainBadge, describeDuration, describeWhen } from "./shared";
import { Picker } from "./Picker";

export interface BridgeProps {
  from: ChainId;
  to: ChainId;
  token: TokenId;
  tokens: TokenId[];
  amount: string;
  recipient: string;
  defaultRecipient: string;
  sourceBalance: bigint | undefined;
  destinationBalance: bigint | undefined;
  fee: BridgeFee | null;
  ism: WiredIsm | null;
  ismError: string | null;
  live: boolean;
  notLive: string | null;
  error: string | null;
  sending: boolean;
  /// Whether the wallet that signs on the origin chain is connected.
  connected: boolean;
  onConnect: () => void;
  /// A passing message about the send in progress.
  notice: string | null;
  /// The connected EVM account is one the relayer delivers from.
  relayerAccount: boolean;
  onPickFrom: (c: ChainId) => void;
  onPickTo: (c: ChainId) => void;
  onFlip: () => void;
  onToken: (t: TokenId) => void;
  onAmount: (a: string) => void;
  onRecipient: (r: string) => void;
  onSend: () => void;
}

export function BridgeView(p: BridgeProps) {
  const source = CHAINS[p.from];
  const destination = CHAINS[p.to];
  const eta = expectedSeconds(p.from);

  // The button says what is missing, and connects the wallet when that is what is missing.
  const wanted = Number(p.amount);
  const over =
    p.sourceBalance !== undefined && p.amount !== "" && wanted > Number(formatAmount(p.sourceBalance, p.token));
  const action: { label: string; run?: () => void } = p.sending
    ? { label: `Confirm in ${walletFor(source)}…` }
    : !p.live
      ? { label: "Route not available" }
      : !p.connected
        ? { label: `Connect ${walletFor(source)}`, run: p.onConnect }
        : !p.amount || !(wanted > 0)
          ? { label: "Enter an amount" }
          : over
            ? { label: `Not enough ${p.token}` }
            : { label: `Bridge ${p.amount} ${p.token} to ${destination.name}`, run: p.onSend };
  const slow = ORIGIN_FINALITY[p.from].seconds >= SLOW_ORIGIN_SECONDS;

  return (
    <div className="bridge-grid">
      <section className="panel send">
        <header className="panel-head">
          <h2>Bridge</h2>
        </header>

        <div className="field">
          <div className="field-top">
            <span>From</span>
            <ChainPicker value={p.from} onChange={p.onPickFrom} />
          </div>
          <div className="field-row">
            <input
              className="amount"
              value={p.amount}
              placeholder="0"
              inputMode="decimal"
              onChange={(e) => p.onAmount(e.target.value)}
              aria-label="Amount"
            />
            <TokenPicker value={p.token} tokens={p.tokens} onChange={p.onToken} />
          </div>
          <div className="field-bottom">
            <span>
              {p.sourceBalance === undefined
                ? `Connect ${walletFor(source)} to see your balance`
                : `Balance ${formatAmount(p.sourceBalance, p.token)} ${p.token}`}
            </span>
            {p.sourceBalance !== undefined && p.sourceBalance > 0n && (
              <button className="max" onClick={() => p.onAmount(formatAmount(p.sourceBalance!, p.token))}>
                Max
              </button>
            )}
          </div>
        </div>

        <div className="flip-row">
          <button className="flip" onClick={p.onFlip} aria-label="Swap direction" title="Swap direction">
            <svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
              <path d="M12 5v14M6 13l6 6 6-6" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          </button>
        </div>

        <div className="field">
          <div className="field-top">
            <span>To</span>
            <ChainPicker value={p.to} onChange={p.onPickTo} />
          </div>
          <div className="field-row">
            <span className={p.amount ? "amount received" : "amount received placeholder"}>{p.amount || "0"}</span>
            <span className="token-static">{p.token}</span>
          </div>
          <div className="field-bottom">
            <span>
              {p.destinationBalance === undefined
                ? `Connect ${walletFor(destination)} to see your balance`
                : `Balance ${formatAmount(p.destinationBalance, p.token)} ${p.token}`}
            </span>
          </div>
        </div>

        <label className="recipient">
          <span>Recipient on {destination.name}</span>
          <input
            value={p.recipient}
            placeholder={p.defaultRecipient || `Address on ${destination.name}`}
            onChange={(e) => p.onRecipient(e.target.value)}
            spellCheck={false}
          />
        </label>

        {slow && (
          <p className="wait">
            Expected {describeWhen(Date.now() + eta * 1000)}, about {describeDuration(eta)} from now.{" "}
            {ORIGIN_FINALITY[p.from].reason}
          </p>
        )}
        {p.relayerAccount && (
          <p className="notice">
            This is the relayer's own account. Transfers from it can collide with the relayer's deliveries. Use a
            different account for testing.
          </p>
        )}
        {!p.live && p.notLive && <p className="notice">{p.notLive}</p>}
        {p.notice && <p className="notice">{p.notice}</p>}
        {p.error && <p className="error">{p.error}</p>}

        <details className="details">
          <summary>
            <span>
              Arrives in about {describeDuration(eta)}
            </span>
            <span className="details-fee">Fee {p.fee ? formatFee(p.fee) : "…"}</span>
          </summary>
          <dl className="summary">
            <dt>Verified by</dt>
            <dd>
              {p.ismError ? (
                <span className="bad">could not read the ISM</span>
              ) : !p.ism ? (
                "…"
              ) : p.ism.url ? (
                <a href={p.ism.url} target="_blank" rel="noreferrer" title={p.ism.id}>
                  {shortId(p.ism.id)}
                </a>
              ) : (
                <code title={p.ism.id}>{shortId(p.ism.id)}</code>
              )}
              <span className="sub">ISM on {destination.name}, which checks the enclave's attestation</span>
            </dd>
          </dl>
        </details>

        <button className="primary" onClick={action.run} disabled={!action.run}>
          {action.label}
        </button>
      </section>

    </div>
  );
}

export function ChainPicker({
  value,
  onChange,
  chains = ALL_CHAINS,
}: {
  value: ChainId;
  onChange: (c: ChainId) => void;
  chains?: ChainId[];
}) {
  return (
    <Picker
      value={value}
      onChange={onChange}
      placeholder="Search chains"
      className="chain-picker"
      options={chains.map((c) => ({
        value: c,
        label: CHAINS[c].name,
        keywords: `${CHAINS[c].domain}`,
        icon: <ChainBadge chain={c} size={24} />,
      }))}
      trigger={
        <>
          <ChainBadge chain={value} size={24} />
          <span>{CHAINS[value].name}</span>
        </>
      }
    />
  );
}

export function TokenPicker({
  value,
  tokens,
  onChange,
}: {
  value: TokenId;
  tokens: TokenId[];
  onChange: (t: TokenId) => void;
}) {
  return (
    <Picker
      value={value}
      onChange={onChange}
      placeholder="Search tokens"
      className="token-picker"
      options={tokens.map((t) => ({ value: t, label: t }))}
      trigger={<span>{value}</span>}
    />
  );
}
