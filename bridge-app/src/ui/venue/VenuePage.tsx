// The venue: trading, launching and liquidity, and the agent docs, under one floating nav.

import { useEffect, useState } from "react";
import type { Transfer } from "../../bridge";
import { fetchInfo } from "../../trade";
import type { TradeInfo } from "../../trade";
import { createStore } from "./store";
import { TradeView } from "./TradeView";
import { LaunchView } from "./LaunchView";
import { DocsView } from "./DocsView";

export type Section = "trade" | "launch" | "docs";

export interface VenueProps {
  evm: string | null;
  cosmos: string | null;
  onConnect: (wallet: "MetaMask" | "Keplr") => void;
  /// A bridge leg was sent, for the History tab.
  onTransfer: (t: Transfer) => void;
  info: TradeInfo | null;
  infoError: string | null;
  reload: () => Promise<void>;
}

const info = createStore<{ info: TradeInfo | null; error: string | null }>({ info: null, error: null });

async function reload(refresh = false) {
  try {
    info.set({ info: await fetchInfo(refresh), error: null });
  } catch (e) {
    info.set({ info: info.get().info, error: e instanceof Error ? e.message : String(e) });
  }
}

const SECTIONS: { id: Section; label: string }[] = [
  { id: "trade", label: "Trade" },
  { id: "launch", label: "Launch & Fund" },
  { id: "docs", label: "Docs (Agents)" },
];

export function VenuePage(p: {
  section: Section;
  onSection: (s: Section) => void;
  evm: string | null;
  cosmos: string | null;
  onConnect: (wallet: "MetaMask" | "Keplr") => void;
  onTransfer: (t: Transfer) => void;
}) {
  const catalog = info.use();
  const [, tick] = useState(0);

  // The catalog changes as tokens launch and pools fill; the API re-reads both every 30s.
  useEffect(() => {
    reload();
    const timer = setInterval(() => reload().then(() => tick((n) => n + 1)), 30_000);
    return () => clearInterval(timer);
  }, []);

  const props: VenueProps = {
    evm: p.evm,
    cosmos: p.cosmos,
    onConnect: p.onConnect,
    onTransfer: p.onTransfer,
    info: catalog.info,
    infoError: catalog.error,
    reload: () => reload(true),
  };

  return (
    <div className="venue">
      <nav className="venue-nav" aria-label="Trade sections">
        {SECTIONS.map((s) => (
          <button
            key={s.id}
            className={p.section === s.id ? "on" : ""}
            aria-current={p.section === s.id ? "page" : undefined}
            onClick={() => p.onSection(s.id)}
          >
            {s.label}
          </button>
        ))}
      </nav>
      {p.section === "trade" ? (
        <TradeView {...props} />
      ) : p.section === "launch" ? (
        <LaunchView {...props} />
      ) : (
        <DocsView />
      )}
    </div>
  );
}
