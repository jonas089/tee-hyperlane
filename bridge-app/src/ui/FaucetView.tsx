// The devnet faucet: a fixed grant of TIA, once per address.
//
// It asks the relayer API rather than signing anything here. The grant comes out of an
// account on the deployment host, so the browser's only job is to name a recipient.

import { useCallback, useEffect, useState } from "react";
import { CHAINS, RELAYER_API } from "../config";
import { ALL_CHAINS, ChainBadge, shorten } from "./shared";

export function FaucetView({
  address,
  onFunded,
  onConnect,
}: {
  address: string | null;
  onFunded: () => void;
  onConnect: () => void;
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

  const grant = amount ?? 1000;

  return (
    <div className="faucet-grid">
      <section className="panel faucet">
        <header className="panel-head">
          <h2>Faucet</h2>
        </header>
        {!enabled ? (
          <p className="notice">The faucet is not configured on this deployment.</p>
        ) : (
          <>
            <div className="grant">
              <span className="grant-amount">{grant}</span>
              <span className="grant-unit">TIA</span>
            </div>
            <p className="hint">On {CHAINS.celestia.name}, once per address.</p>

            <label className="recipient">
              <span>Recipient</span>
              <input readOnly value={address ?? ""} placeholder="Connect Keplr to claim" />
            </label>

            {!address ? (
              <button className="primary" onClick={onConnect}>
                Connect Keplr
              </button>
            ) : claimed && !txHash ? (
              <p className="notice">This address has already claimed.</p>
            ) : (
              <button className="primary" disabled={busy || (claimed && !txHash)} onClick={claim}>
                {busy ? "Sending…" : `Claim ${grant} TIA`}
              </button>
            )}

            {txHash && (
              <p className="success">
                Sent in{" "}
                <a href={`/tx/${txHash}`} target="_blank" rel="noreferrer">
                  {shorten(txHash, 8)}
                </a>
                . It lands in the next block.
              </p>
            )}
            {error && <p className="error">{error}</p>}
          </>
        )}
      </section>

      <section className="panel explain">
        <header className="panel-head">
          <h2>Then try a route</h2>
        </header>
        <p className="hint">
          Every route has {CHAINS.celestia.name} on one side. TIA leaves Celestia as collateral and arrives as a
          synthetic on the other chain; sending it back burns the synthetic and releases the collateral.
        </p>
        <ul className="route-list">
          {ALL_CHAINS.filter((c) => c !== "celestia").map((c) => (
            <li key={c}>
              <ChainBadge chain="celestia" size={26} />
              <span className="route-line" />
              <ChainBadge chain={c} size={26} />
              <span>{CHAINS[c].name}</span>
            </li>
          ))}
        </ul>
      </section>
    </div>
  );
}
