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
import {
  CELESTIA_DENOM,
  CHAINS,
  DECIMALS,
  RELAYER_API,
  REMOTE_ROUTER_GAS,
  routerFor,
} from "./config";
import type { Chain, ChainId, EvmChain, TokenId } from "./config";
import { switchEvmChain } from "./wallets";

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
  /** When the origin accepted it, and when the destination was first seen to have it. */
  sentAt: number;
  deliveredAt?: number;
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

  // The wallet follows the chain it was connected on, not the one picked in the form, and a
  // router address means something different on every chain. Sending without this put a
  // Base router address into a Sepolia transaction: no contract there, so MetaMask sent the
  // delivery fee to a bare address as a plain transfer. It succeeded, it cost real money, and
  // it bridged nothing.
  await switchEvmChain(opts.chain);

  // Switching can be declined, and a declined switch looks like success to the code above.
  // Ask the wallet where it actually is rather than assuming it moved.
  const current = (await window.ethereum!.request({ method: "eth_chainId" })) as string;
  if (current.toLowerCase() !== opts.chain.chainIdHex.toLowerCase()) {
    throw new Error(
      `wallet is on chain ${current}, not ${opts.chain.name} (${opts.chain.chainIdHex}) - ` +
        "approve the network switch and try again",
    );
  }

  // Cheap last guard: on the right chain this address is a contract. If it has no code we are
  // about to repeat the same mistake in a new disguise.
  const code = await rpc(opts.chain, "eth_getCode", [router, "latest"]);
  if (!code || code === "0x") {
    throw new Error(`no router deployed at ${router} on ${opts.chain.name}`);
  }

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
  // The gateway path is /hyperlane/v1/..., not /hyperlane/core/v1/... - the latter answers
  // 501, which would leave every Celestia-bound transfer stuck showing "not delivered".
  const response = await fetch(
    `${chain.rest}/hyperlane/v1/mailboxes/${chain.mailboxId}/delivered/${messageId}`,
  );
  if (!response.ok) return false;
  const body = await response.json();
  return Boolean(body.delivered);
}

/** What the sender pays the paymaster for delivery on the far side. */
export interface BridgeFee {
  amount: bigint;
  symbol: string;
  decimals: number;
}

/// Quoted from the chain rather than estimated, because the oracle moves it hourly and a
/// stale number is the difference between a transfer that lands and one that reverts.
export async function quoteBridgeFee(from: ChainId, to: ChainId): Promise<BridgeFee> {
  const origin = CHAINS[from];
  const destination = CHAINS[to];

  if (origin.kind === "cosmos") {
    const response = await fetch(
      `${origin.rest}/hyperlane/v1/igps/${origin.igpId}/quote_gas_payment` +
        `?destination_domain=${destination.domain}&gas_limit=${REMOTE_ROUTER_GAS}`,
    );
    if (!response.ok) throw new Error(`fee quote returned ${response.status}`);
    const body = await response.json();
    const amount = body?.gas_payment?.[0]?.amount ?? "0";
    return { amount: BigInt(amount), symbol: "TIA", decimals: 6 };
  }

  const router = routerFor("TIA", origin.id) ?? routerFor("USDC", origin.id);
  if (!router) throw new Error(`no router on ${origin.name}`);
  const data = encodeFunctionData({
    abi: ROUTER_ABI,
    functionName: "quoteGasPayment",
    args: [destination.domain],
  });
  const result = await ethCall(origin, router, data);
  return { amount: BigInt(result), symbol: "ETH", decimals: 18 };
}

/**
 * Enough digits to see the number, rather than a fixed six.
 *
 * A Base delivery costs 0.000000508755114154 ETH. Truncated to six decimals every digit of
 * that is a zero, so the UI said "0 ETH" for a fee it was about to charge - which reads as
 * "free" rather than "small". Significant digits are counted from the first non-zero, so a
 * fee is never rounded away to nothing however small it is.
 */
export function formatFee(fee: BridgeFee): string {
  const unit = 10n ** BigInt(fee.decimals);
  const whole = fee.amount / unit;
  const raw = (fee.amount % unit).toString().padStart(fee.decimals, "0");

  if (whole > 0n) {
    const fraction = raw.slice(0, 6).replace(/0+$/, "");
    return `${fraction ? `${whole}.${fraction}` : whole} ${fee.symbol}`;
  }
  if (fee.amount === 0n) return `0 ${fee.symbol}`;

  const firstDigit = raw.search(/[1-9]/);
  const fraction = raw.slice(0, firstDigit + 4).replace(/0+$/, "");
  return `0.${fraction} ${fee.symbol}`;
}

/**
 * What a wallet or node actually said went wrong.
 *
 * MetaMask rejects with a plain object, not an `Error`, so `String(e)` on it produces
 * "[object Object]" and the user is told nothing at all. The useful text is in one of several
 * places depending on whether the wallet, the node or the contract refused.
 */
export function describeError(e: unknown): string {
  if (e instanceof Error && e.message) return e.message;
  if (typeof e === "string") return e;
  if (e && typeof e === "object") {
    const any = e as Record<string, any>;
    const nested =
      any.data?.message ?? any.error?.message ?? any.cause?.message ?? any.shortMessage;
    const text = nested ?? any.message ?? any.reason;
    if (typeof text === "string" && text) {
      return any.code !== undefined ? `${text} (code ${any.code})` : text;
    }
    try {
      return JSON.stringify(e);
    } catch {
      return "the wallet refused without saying why";
    }
  }
  return String(e);
}

/** What this account holds of one token on one chain. */
export async function fetchBalance(
  chain: Chain,
  token: TokenId,
  address: string,
): Promise<bigint> {
  if (chain.kind === "evm") {
    const router = routerFor(token, chain.id);
    if (!router) return 0n;
    const data = encodeFunctionData({
      abi: ROUTER_ABI,
      functionName: "balanceOf",
      args: [address as `0x${string}`],
    });
    return BigInt(await ethCall(chain, router, data));
  }
  const denom = CELESTIA_DENOM[token];
  const response = await fetch(
    `${chain.rest}/cosmos/bank/v1beta1/balances/${address}/by_denom?denom=${encodeURIComponent(denom)}`,
  );
  if (!response.ok) return 0n;
  const body = await response.json();
  return BigInt(body?.balance?.amount ?? "0");
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
      next.deliveredAt = next.deliveredAt ?? Date.now();
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
