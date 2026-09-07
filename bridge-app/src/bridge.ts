// Sending a transfer, and following it to the other side.
//
// A transfer has four observable steps, and the UI shows exactly those:
//   dispatched  the origin chain accepted it and put its id in the merkle tree
//   attested    an enclave verified the origin state that contains it
//   authorised  the destination ISM accepted the proofs and allowed this id
//   delivered   the destination mailbox processed it
//
// "Attested" is the step that distinguishes this bridge, so it carries the quote.

import { encodeFunctionData, parseAbi, toHex } from "viem";
import { CHAINS, DECIMALS, RELAYER_API, routerFor } from "./config";
import type { ChainId, EvmChain, TokenId } from "./config";

export type Step = "dispatched" | "attested" | "authorised" | "delivered";
export const STEPS: Step[] = ["dispatched", "attested", "authorised", "delivered"];

export interface Transfer {
  messageId: string;
  token: TokenId;
  amount: string;
  from: ChainId;
  to: ChainId;
  originTx: string;
  /** How far it has got. */
  reached: Step;
  /** Present once an enclave has attested the batch this message is in. */
  attestation?: Attestation;
  deliveryTx?: string;
  failure?: string;
}

/** What an enclave signed for the batch containing a message. */
export interface Attestation {
  /** Origin block whose state was attested. */
  height: number;
  stateRoot: string;
  /** Hex TDX quote. */
  quote: string;
  /** The enclave measurements the quote carries. */
  measurements: {
    mrTd: string;
    osImageHash: string;
    composeHash: string;
  };
  /** Message ids authorised alongside this one. */
  batch: string[];
}

const ROUTER_ABI = parseAbi([
  "function transferRemote(uint32 destination, bytes32 recipient, uint256 amount) payable returns (bytes32)",
  "function quoteGasPayment(uint32 destination) view returns (uint256)",
  "function balanceOf(address owner) view returns (uint256)",
  "function approve(address spender, uint256 amount) returns (bool)",
  "function allowance(address owner, address spender) view returns (uint256)",
]);

const MAILBOX_ABI = parseAbi(["function delivered(bytes32 id) view returns (bool)"]);

export function toBaseUnits(amount: string, token: TokenId): bigint {
  const [whole, fraction = ""] = amount.trim().split(".");
  const decimals = DECIMALS[token];
  const padded = (fraction + "0".repeat(decimals)).slice(0, decimals);
  return BigInt(whole || "0") * 10n ** BigInt(decimals) + BigInt(padded || "0");
}

export function formatAmount(base: bigint, token: TokenId): string {
  const decimals = DECIMALS[token];
  const unit = 10n ** BigInt(decimals);
  const whole = base / unit;
  const fraction = (base % unit).toString().padStart(decimals, "0").replace(/0+$/, "");
  return fraction ? `${whole}.${fraction}` : `${whole}`;
}

/** Hyperlane addresses are bytes32. EVM addresses are left-padded; bech32 uses the account id. */
export function toRecipientBytes32(address: string): `0x${string}` {
  if (address.startsWith("0x")) {
    return `0x${address.slice(2).toLowerCase().padStart(64, "0")}`;
  }
  return `0x${bech32AccountId(address).padStart(64, "0")}`;
}

const BECH32_ALPHABET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";

function bech32AccountId(address: string): string {
  const data = address.slice(address.lastIndexOf("1") + 1);
  const values = [...data].map((c) => BECH32_ALPHABET.indexOf(c)).slice(0, -6);
  let acc = 0;
  let bits = 0;
  const out: number[] = [];
  for (const value of values) {
    acc = (acc << 5) | value;
    bits += 5;
    while (bits >= 8) {
      bits -= 8;
      out.push((acc >> bits) & 0xff);
    }
  }
  return out.map((b) => b.toString(16).padStart(2, "0")).join("");
}

async function rpc(chain: EvmChain, method: string, params: unknown[]): Promise<any> {
  const response = await fetch(chain.rpc, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
  const body = await response.json();
  if (body.error) throw new Error(body.error.message ?? "rpc error");
  return body.result;
}

async function ethCall(chain: EvmChain, to: string, data: string): Promise<string> {
  return rpc(chain, "eth_call", [{ to, data }, "latest"]);
}

export async function quoteFee(chain: EvmChain, token: TokenId, destination: number) {
  const router = routerFor(token, chain.id);
  if (!router) throw new Error(`${token} is not deployed on ${chain.name}`);
  const data = encodeFunctionData({
    abi: ROUTER_ABI,
    functionName: "quoteGasPayment",
    args: [destination],
  });
  return BigInt(await ethCall(chain, router, data));
}

/** Send from an EVM chain. Returns the transaction hash; the id arrives with the receipt. */
export async function sendFromEvm(opts: {
  chain: EvmChain;
  token: TokenId;
  destination: number;
  recipient: string;
  amount: bigint;
  sender: string;
}): Promise<string> {
  const router = routerFor(opts.token, opts.chain.id);
  if (!router) throw new Error(`${opts.token} is not deployed on ${opts.chain.name}`);
  const fee = await quoteFee(opts.chain, opts.token, opts.destination);

  const data = encodeFunctionData({
    abi: ROUTER_ABI,
    functionName: "transferRemote",
    args: [opts.destination, toRecipientBytes32(opts.recipient), opts.amount],
  });

  return window.ethereum!.request({
    method: "eth_sendTransaction",
    params: [{ from: opts.sender, to: router, data, value: toHex(fee) }],
  });
}

/** The message id the mailbox emitted, read back from the receipt. */
const DISPATCH_ID_TOPIC =
  "0x788dbc1b7152732178210e7f4d9d010ef016f9eafbe66786bd7169f56e0c353a";

export async function messageIdFromReceipt(
  chain: EvmChain,
  txHash: string,
): Promise<string | null> {
  const receipt = await rpc(chain, "eth_getTransactionReceipt", [txHash]);
  if (!receipt) return null;
  const log = receipt.logs.find(
    (l: any) => l.topics[0] === DISPATCH_ID_TOPIC && l.topics.length === 2,
  );
  return log?.topics[1] ?? null;
}

/** Whether the destination mailbox has processed this message. This is the real check. */
export async function isDelivered(destination: ChainId, messageId: string): Promise<boolean> {
  const chain = CHAINS[destination];
  if (chain.kind === "evm") {
    const data = encodeFunctionData({
      abi: MAILBOX_ABI,
      functionName: "delivered",
      args: [messageId as `0x${string}`],
    });
    const result = await ethCall(chain, chain.mailbox, data);
    return BigInt(result) === 1n;
  }
  // hyperlane-cosmos removes an id from the ISM's authorised set once it is processed, so a
  // delivered message is one the chain knows and no longer holds.
  const response = await fetch(
    `${chain.rest}/hyperlane/core/v1/delivered/${chain.mailboxId}/${messageId}`,
  );
  if (!response.ok) return false;
  const body = await response.json();
  return Boolean(body.delivered);
}

/** Ask the coprocessor which batch a message landed in, and what the enclave signed for it. */
export async function fetchAttestation(messageId: string): Promise<Attestation | null> {
  const response = await fetch(`${RELAYER_API}/attestation/${messageId}`);
  if (!response.ok) return null;
  return (await response.json()) as Attestation;
}

/** Advance a transfer's state. Called on demand, not on a timer. */
export async function refresh(transfer: Transfer): Promise<Transfer> {
  const next = { ...transfer };
  try {
    if (await isDelivered(transfer.to, transfer.messageId)) {
      next.reached = "delivered";
    }
    const attestation = await fetchAttestation(transfer.messageId);
    if (attestation) {
      next.attestation = attestation;
      if (next.reached === "dispatched") next.reached = "attested";
    }
    next.failure = undefined;
  } catch (error) {
    next.failure = error instanceof Error ? error.message : String(error);
  }
  return next;
}
