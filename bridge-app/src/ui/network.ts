// Looking transfers up on the relayer, for searches this browser's history cannot answer.

import { RELAYER_API } from "../config";

/// A transfer as the relayer records it, for search results this browser did not send.
export interface RemoteTransfer {
  id: string;
  route: string;
  status: string;
  /** The origin router, as a 32-byte Hyperlane address. */
  sender: string;
  transfer: { recipient: string; amount: string } | null;
  dispatch: { tx: string; block: number; timestamp: number; from: string } | null;
  delivery: { at: number; tx: string } | null;
  origin: { name: string; domain: number };
  destination: { name: string; domain: number };
  waitingOn: string | null;
  problem: string | null;
  failure: string | null;
  links: { dispatchTx: string | null; deliveryTx: string | null };
}

export async function searchRelayer(q: string): Promise<RemoteTransfer[]> {
  const r = await fetch(`${RELAYER_API}/v1/search?q=${encodeURIComponent(q)}`);
  if (!r.ok) return [];
  const body = await r.json();
  return Array.isArray(body.messages) ? body.messages : [];
}
