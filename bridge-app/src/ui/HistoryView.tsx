// Every transfer this browser sent, searchable, with the selected one in full on the right.
// A search the local list cannot answer is asked of the relayer, so a transfer sent from
// anywhere can be found by its transaction hash, message id or an address.

import { useEffect, useMemo, useState } from "react";
import { CHAINS, ROUTERS, expectedSeconds } from "../config";
import type { ChainId, TokenId } from "../config";
import { formatAmount } from "../bridge";
import type { Transfer } from "../bridge";
import { searchRelayer } from "./network";
import type { RemoteTransfer } from "./network";
import {
  ALL_CHAINS,
  ChainBadge,
  Copy,
  StatusChip,
  StepTrack,
  describeAgo,
  describeWhen,
  explorerTx,
  shorten,
  statusOf,
} from "./shared";
import type { Status } from "./shared";

type Filter = "all" | Status;

const FILTERS: { id: Filter; label: string }[] = [
  { id: "all", label: "All" },
  { id: "flight", label: "In flight" },
  { id: "delivered", label: "Delivered" },
  { id: "failed", label: "Failed" },
];

export function HistoryView({
  transfers,
  onRefresh,
  relayerUrl,
  initialQuery = "",
}: {
  transfers: Transfer[];
  onRefresh: (id: string) => Promise<void>;
  relayerUrl: string;
  initialQuery?: string;
}) {
  const [query, setQuery] = useState(initialQuery);
  const [filter, setFilter] = useState<Filter>("all");
  const [chain, setChain] = useState<ChainId | "any">("any");
  const [selected, setSelected] = useState<string | null>(transfers[0]?.messageId ?? null);
  const [remote, setRemote] = useState<RemoteTransfer[]>([]);
  const [searching, setSearching] = useState(false);

  const q = query.trim().toLowerCase();
  const shown = useMemo(
    () =>
      transfers.filter((t) => {
        if (filter !== "all" && statusOf(t) !== filter) return false;
        if (chain !== "any" && t.from !== chain && t.to !== chain) return false;
        if (!q) return true;
        return [t.messageId, t.originTx, t.deliveryTx ?? "", CHAINS[t.from].name, CHAINS[t.to].name, t.token]
          .some((v) => v.toLowerCase().includes(q));
      }),
    [transfers, filter, chain, q],
  );

  // Only for something that looks like a hash or an address, and only after typing stops.
  useEffect(() => {
    setRemote([]);
    if (q.length < 10) return;
    let live = true;
    setSearching(true);
    const timer = setTimeout(() => {
      searchRelayer(query.trim())
        .then((found) => {
          if (!live) return;
          const known = new Set(transfers.map((t) => t.messageId.toLowerCase()));
          setRemote(found.filter((r) => !known.has(r.id.toLowerCase())));
        })
        .catch(() => {})
        .finally(() => live && setSearching(false));
    }, 350);
    return () => {
      live = false;
      clearTimeout(timer);
      setSearching(false);
    };
  }, [q, query, transfers]);

  const active =
    shown.find((t) => t.messageId === selected) ??
    remote.find((r) => r.id === selected) ??
    shown[0] ??
    remote[0] ??
    null;

  return (
    <div className="history-grid">
      <div className="toolbar">
        <div className="search">
          <svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
            <circle cx="11" cy="11" r="7" fill="none" stroke="currentColor" strokeWidth="2" />
            <path d="M20 20l-3.5-3.5" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
          </svg>
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search by transaction hash, message id or address"
            spellCheck={false}
            autoFocus
          />
          {query && (
            <button className="clear" onClick={() => setQuery("")} aria-label="Clear search">
              ×
            </button>
          )}
        </div>
        <div className="filters">
          {FILTERS.map((f) => (
            <button key={f.id} className={filter === f.id ? "chip on" : "chip"} onClick={() => setFilter(f.id)}>
              {f.label}
            </button>
          ))}
          <select value={chain} onChange={(e) => setChain(e.target.value as ChainId | "any")} aria-label="Chain">
            <option value="any">Any chain</option>
            {ALL_CHAINS.map((c) => (
              <option key={c} value={c}>
                {CHAINS[c].name}
              </option>
            ))}
          </select>
        </div>
      </div>

      <section className="panel list-pane">
        <header className="panel-head">
          <h2>Transfers</h2>
          <span className="muted">
            {shown.length} of {transfers.length}
          </span>
        </header>
        {transfers.length === 0 && !q ? (
          <p className="hint">
            Nothing sent from this browser yet. Paste a transaction hash or message id above to look up any
            transfer the relayer has seen.
          </p>
        ) : (
          <ul className="rows">
            {shown.map((t) => (
              <li
                key={t.messageId}
                className={active && "messageId" in active && active.messageId === t.messageId ? "row on" : "row"}
                onClick={() => setSelected(t.messageId)}
              >
                <span className="row-route">
                  <ChainBadge chain={t.from} size={26} />
                  <ChainBadge chain={t.to} size={26} />
                </span>
                <span className="row-main">
                  <strong>
                    {t.amount} {t.token}
                  </strong>
                  <span className="muted">
                    {CHAINS[t.from].name} → {CHAINS[t.to].name}
                  </span>
                </span>
                <span className="row-when muted">{describeAgo(t.sentAt)}</span>
                <StatusChip status={statusOf(t)} />
              </li>
            ))}
            {remote.length > 0 && <li className="divider">Found on the relayer</li>}
            {remote.map((r) => {
              const [from, to] = routeChains(r.route);
              return (
                <li
                  key={r.id}
                  className={active && "id" in active && active.id === r.id ? "row on" : "row"}
                  onClick={() => setSelected(r.id)}
                >
                  <span className="row-route">
                    <ChainBadge chain={from} size={26} />
                    <ChainBadge chain={to} size={26} />
                  </span>
                  <span className="row-main">
                    <strong>{remoteAmount(r)}</strong>
                    <span className="muted">
                      {CHAINS[from].name} → {CHAINS[to].name}
                    </span>
                  </span>
                  <span className="row-when muted">{r.dispatch ? describeAgo(r.dispatch.timestamp * 1000) : ""}</span>
                  <StatusChip status={remoteStatus(r)} />
                </li>
              );
            })}
            {shown.length === 0 && remote.length === 0 && (
              <li className="empty">{searching ? "Searching the relayer…" : "No transfer matches."}</li>
            )}
          </ul>
        )}
      </section>

      <section className="panel detail-pane">
        {!active ? (
          <div className="empty-detail">
            <p>Select a transfer to see every step it took.</p>
          </div>
        ) : "messageId" in active ? (
          <LocalDetail transfer={active} onRefresh={onRefresh} />
        ) : (
          <RemoteDetail record={active} relayerUrl={relayerUrl} />
        )}
      </section>
    </div>
  );
}

function LocalDetail({ transfer: t, onRefresh }: { transfer: Transfer; onRefresh: (id: string) => Promise<void> }) {
  const [checking, setChecking] = useState(false);
  const [quoteOpen, setQuoteOpen] = useState(false);
  const expected = t.sentAt + expectedSeconds(t.from) * 1000;
  return (
    <>
      <header className="detail-head">
        <div>
          <span className="detail-amount">
            {t.amount} {t.token}
          </span>
          <span className="detail-route">
            <ChainBadge chain={t.from} size={22} /> {CHAINS[t.from].name}
            <span className="arrow">→</span>
            <ChainBadge chain={t.to} size={22} /> {CHAINS[t.to].name}
          </span>
        </div>
        <StatusChip status={statusOf(t)} />
      </header>

      <StepTrack reached={t.reached} />

      <dl className="facts">
        <dt>Sent</dt>
        <dd>{describeWhen(t.sentAt)}</dd>
        <dt>{t.reached === "delivered" ? "Landed" : "Expected"}</dt>
        <dd>{describeWhen(t.reached === "delivered" ? (t.deliveredAt ?? Date.now()) : expected)}</dd>
        <dt>Message id</dt>
        <dd>
          <Copy value={t.messageId}>
            <code>{shorten(t.messageId, 12)}</code>
          </Copy>
        </dd>
        <dt>Origin transaction</dt>
        <dd>
          <a href={explorerTx(t.from, t.originTx)} target="_blank" rel="noreferrer">
            <code>{shorten(t.originTx, 12)}</code>
          </a>
        </dd>
        {t.deliveryTx && (
          <>
            <dt>Delivery transaction</dt>
            <dd>
              <a href={explorerTx(t.to, t.deliveryTx)} target="_blank" rel="noreferrer">
                <code>{shorten(t.deliveryTx, 12)}</code>
              </a>
            </dd>
          </>
        )}
      </dl>

      {t.failure && <p className="error">{t.failure}</p>}

      {t.attestation ? (
        <div className="attest">
          <h3>Attestation</h3>
          <dl className="facts">
            <dt>Origin block</dt>
            <dd>{t.attestation.height.toLocaleString()}</dd>
            <dt>State root</dt>
            <dd>
              <Copy value={t.attestation.stateRoot}>
                <code>{shorten(t.attestation.stateRoot, 12)}</code>
              </Copy>
            </dd>
            <dt>Enclave image</dt>
            <dd>
              <code>{shorten(t.attestation.measurements.composeHash, 12)}</code>
            </dd>
            <dt>Batch</dt>
            <dd>
              {t.attestation.batch.length} message{t.attestation.batch.length === 1 ? "" : "s"} attested together
            </dd>
          </dl>
          <button className="linklike" onClick={() => setQuoteOpen(!quoteOpen)}>
            {quoteOpen ? "Hide the TDX quote" : "Show the TDX quote"}
          </button>
          {quoteOpen && <pre className="quote">{t.attestation.quote}</pre>}
        </div>
      ) : (
        <p className="hint">The attestation appears here once the enclave has covered this transfer.</p>
      )}

      <button
        className="secondary"
        disabled={checking}
        onClick={async () => {
          setChecking(true);
          try {
            await onRefresh(t.messageId);
          } finally {
            setChecking(false);
          }
        }}
      >
        {checking ? "Checking…" : "Check status"}
      </button>
    </>
  );
}

function RemoteDetail({ record: r, relayerUrl }: { record: RemoteTransfer; relayerUrl: string }) {
  const [from, to] = routeChains(r.route);
  return (
    <>
      <header className="detail-head">
        <div>
          <span className="detail-amount">{remoteAmount(r)}</span>
          <span className="detail-route">
            <ChainBadge chain={from} size={22} /> {CHAINS[from].name}
            <span className="arrow">→</span>
            <ChainBadge chain={to} size={22} /> {CHAINS[to].name}
          </span>
        </div>
        <StatusChip status={remoteStatus(r)} />
      </header>
      <p className="hint">Found on the relayer. It was not sent from this browser.</p>
      <dl className="facts">
        {r.dispatch && (
          <>
            <dt>Sent</dt>
            <dd>{describeWhen(r.dispatch.timestamp * 1000)}</dd>
          </>
        )}
        {r.delivery && (
          <>
            <dt>Landed</dt>
            <dd>{describeWhen(r.delivery.at * 1000)}</dd>
          </>
        )}
        <dt>Message id</dt>
        <dd>
          <Copy value={r.id}>
            <code>{shorten(r.id, 12)}</code>
          </Copy>
        </dd>
        {r.dispatch && (
          <>
            <dt>Origin transaction</dt>
            <dd>
              <a href={r.links.dispatchTx ?? explorerTx(from, r.dispatch.tx)} target="_blank" rel="noreferrer">
                <code>{shorten(r.dispatch.tx, 12)}</code>
              </a>
            </dd>
          </>
        )}
        {r.transfer && (
          <>
            <dt>Recipient</dt>
            <dd>
              <code>{shorten(r.transfer.recipient, 12)}</code>
            </dd>
          </>
        )}
        {(r.waitingOn || r.problem || r.failure) && (
          <>
            <dt>Status</dt>
            <dd>{r.failure ?? r.problem ?? `Waiting on ${r.waitingOn}`}</dd>
          </>
        )}
      </dl>
      <a className="secondary" href={`${relayerUrl}#/message/${r.id}`} target="_blank" rel="noreferrer">
        Open in the relayer explorer
      </a>
    </>
  );
}

/// `base-to-celestia` → `["base", "celestia"]`.
function routeChains(route: string): [ChainId, ChainId] {
  const [a, b] = route.split("-to-") as [ChainId, ChainId];
  return [ALL_CHAINS.includes(a) ? a : "celestia", ALL_CHAINS.includes(b) ? b : "celestia"];
}

function remoteStatus(r: RemoteTransfer): Status {
  if (r.status === "delivered") return "delivered";
  if (r.status === "failed" || r.status === "overdue") return "failed";
  return "flight";
}

/// Which token a relayer record moved, from the router that sent it.
function remoteToken(r: RemoteTransfer): TokenId | null {
  const [from] = routeChains(r.route);
  const sender = (r.sender ?? "").toLowerCase();
  for (const token of Object.keys(ROUTERS) as TokenId[]) {
    const router = ROUTERS[token][from]?.toLowerCase().replace(/^0x/, "");
    if (router && sender.endsWith(router)) return token;
  }
  return null;
}

function remoteAmount(r: RemoteTransfer): string {
  if (!r.transfer) return "Transfer";
  const token = remoteToken(r);
  return token ? `${formatAmount(BigInt(r.transfer.amount), token)} ${token}` : `${r.transfer.amount} base units`;
}
