// Small pieces every view uses: formatting, chain badges, status chips.

import type { ReactNode } from "react";
import { CHAINS } from "../config";
import type { ChainId } from "../config";
import { STEPS } from "../bridge";
import type { Step, Transfer } from "../bridge";

export const ALL_CHAINS: ChainId[] = ["celestia", "sepolia", "arbitrum", "base", "eden"];

export const STEP_LABEL: Record<Step, string> = {
  dispatched: "Dispatched",
  attested: "Attested in the enclave",
  authorised: "Verified by the ISM",
  delivered: "Delivered",
};

/// Two letters per chain, for the round badge.
const MONOGRAM: Record<ChainId, string> = {
  celestia: "Ce",
  sepolia: "Et",
  arbitrum: "Ar",
  base: "Ba",
  eden: "Ed",
};

export function ChainBadge({ chain, size = 32 }: { chain: ChainId; size?: number }) {
  return (
    <span
      className={`chain-badge chain-${chain}`}
      style={{ width: size, height: size, fontSize: size * 0.38 }}
      aria-hidden="true"
    >
      {MONOGRAM[chain]}
    </span>
  );
}

export type Status = "delivered" | "flight" | "failed";

export function statusOf(t: Transfer): Status {
  if (t.reached === "delivered") return "delivered";
  if (t.failure) return "failed";
  return "flight";
}

const STATUS_LABEL: Record<Status, string> = {
  delivered: "Delivered",
  flight: "In flight",
  failed: "Failed",
};

export function StatusChip({ status }: { status: Status }) {
  return <span className={`status status-${status}`}>{STATUS_LABEL[status]}</span>;
}

/// The four steps as a track, filled up to where the transfer has got.
export function StepTrack({ reached }: { reached: Step }) {
  const at = STEPS.indexOf(reached);
  return (
    <ol className="step-track">
      {STEPS.map((step, i) => (
        <li key={step} className={i <= at ? "done" : i === at + 1 ? "next" : ""}>
          <span className="dot" />
          <span className="label">{STEP_LABEL[step]}</span>
        </li>
      ))}
    </ol>
  );
}

export function chainName(chain: ChainId): string {
  return CHAINS[chain].name;
}

/// Times within a day are a clock reading; anything further out needs the date.
export function describeWhen(at: number): string {
  const withinADay = Math.abs(at - Date.now()) < 24 * 60 * 60 * 1000;
  return new Date(at).toLocaleString([], {
    hour: "2-digit",
    minute: "2-digit",
    ...(withinADay ? {} : { month: "short", day: "numeric" }),
  });
}

export function describeDuration(seconds: number): string {
  if (seconds < 90) return `${Math.round(seconds)} seconds`;
  if (seconds < 90 * 60) return `${Math.round(seconds / 60)} minutes`;
  if (seconds < 36 * 60 * 60) return `${Math.round(seconds / 3600)} hours`;
  return `${Math.round(seconds / 86400)} days`;
}

export function describeAgo(at: number): string {
  const s = Math.max(0, (Date.now() - at) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

export function shorten(value: string | undefined | null, keep = 6): string {
  if (!value) return "";
  if (value.length <= keep * 2 + 2) return value;
  return `${value.slice(0, keep + 2)}…${value.slice(-keep)}`;
}

export function explorerTx(chain: ChainId, tx: string): string {
  return `${CHAINS[chain].explorer}/tx/${tx}`;
}

/// Copies on click and says so for a moment.
export function Copy({ value, children }: { value: string; children: ReactNode }) {
  return (
    <button
      className="copy"
      title="Copy"
      onClick={(e) => {
        navigator.clipboard?.writeText(value).catch(() => {});
        const el = e.currentTarget;
        el.classList.add("copied");
        setTimeout(() => el.classList.remove("copied"), 1200);
      }}
    >
      {children}
    </button>
  );
}
