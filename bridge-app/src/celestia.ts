// Sending a warp transfer from Celestia.
//
// Keplr signs, but it needs the message encoded first, and `MsgRemoteTransfer` is not a type
// CosmJS ships. Rather than pull in a protobuf toolchain for one message, it is encoded by
// hand here - the wire format is nine scalar fields, and writing it out makes the field
// numbers visible instead of hiding them in generated code.

import { Registry } from "@cosmjs/proto-signing";
import { SigningStargateClient } from "@cosmjs/stargate";
import type { CosmosChain, TokenId } from "./config";
import { DECIMALS } from "./config";

const MSG_REMOTE_TRANSFER = "/hyperlane.warp.v1.MsgRemoteTransfer";

export interface RemoteTransfer {
  sender: string;
  tokenId: string;
  destinationDomain: number;
  /** bytes32, hex. */
  recipient: string;
  amount: string;
  maxFee: { denom: string; amount: string };
}

function tag(field: number, wireType: number): number[] {
  return varint((field << 3) | wireType);
}

function varint(value: number): number[] {
  const out: number[] = [];
  let n = value;
  while (n > 0x7f) {
    out.push((n & 0x7f) | 0x80);
    n >>>= 7;
  }
  out.push(n);
  return out;
}

function lengthDelimited(field: number, bytes: Uint8Array | number[]): number[] {
  return [...tag(field, 2), ...varint(bytes.length), ...bytes];
}

function stringField(field: number, value: string): number[] {
  return value ? lengthDelimited(field, [...new TextEncoder().encode(value)]) : [];
}

/** `cosmos.base.v1beta1.Coin`: denom = 1, amount = 2. */
function encodeCoin(coin: { denom: string; amount: string }): number[] {
  return [...stringField(1, coin.denom), ...stringField(2, coin.amount)];
}

/**
 * Field numbers, taken from the message as the chain itself reports it:
 * sender 1, token_id 2, destination_domain 3, recipient 4, amount 5,
 * custom_hook_id 6, gas_limit 7, max_fee 8, custom_hook_metadata 9.
 */
export function encodeRemoteTransfer(msg: RemoteTransfer): Uint8Array {
  const body = [
    ...stringField(1, msg.sender),
    ...stringField(2, msg.tokenId),
    ...tag(3, 0),
    ...varint(msg.destinationDomain),
    ...stringField(4, msg.recipient),
    ...stringField(5, msg.amount),
    // gas_limit is an Int carried as a string; "0" means "use the route's configured limit".
    ...stringField(7, "0"),
    ...lengthDelimited(8, encodeCoin(msg.maxFee)),
  ];
  return new Uint8Array(body);
}

function uintField(field: number, value: number): number[] {
  return value ? [...tag(field, 0), ...varint(value)] : [];
}

function boolField(field: number, value: boolean): number[] {
  return value ? [...tag(field, 0), 1] : [];
}

/// The hub messages a launch and a trade send, as the trade API returns them: proto JSON
/// field names, every field a string, number or bool. Field numbers are from the
/// hyperlane-cosmos protos.
const HUB_ENCODERS: Record<string, (v: any) => number[]> = {
  "/hyperlane.warp.v1.MsgCreateSyntheticToken": (v) => [
    ...stringField(1, v.owner),
    ...stringField(2, v.origin_mailbox),
  ],
  "/hyperlane.warp.v1.MsgSetToken": (v) => [
    ...stringField(1, v.owner),
    ...stringField(2, v.token_id),
    ...stringField(3, v.new_owner ?? ""),
    ...stringField(4, v.ism_id ?? ""),
    ...boolField(7, Boolean(v.renounce_ownership)),
  ],
  "/hyperlane.warp.v1.MsgEnrollRemoteRouter": (v) => [
    ...stringField(1, v.owner),
    ...stringField(2, v.token_id),
    ...lengthDelimited(3, [
      ...uintField(1, Number(v.remote_router.receiver_domain)),
      ...stringField(2, v.remote_router.receiver_contract),
      ...stringField(3, String(v.remote_router.gas)),
    ]),
  ],
  "/hyperlane.warp.v1.MsgUnrollRemoteRouter": (v) => [
    ...stringField(1, v.owner),
    ...stringField(2, v.token_id),
    ...uintField(3, Number(v.receiver_domain)),
  ],
  "/hyperlane.core.v1.MsgProcessMessage": (v) => [
    ...stringField(1, v.mailbox_id),
    ...stringField(2, v.relayer),
    ...stringField(3, v.metadata),
    ...stringField(4, v.message),
  ],
  [MSG_REMOTE_TRANSFER]: (v) => [
    ...encodeRemoteTransfer({
      sender: v.sender,
      tokenId: v.token_id,
      destinationDomain: Number(v.destination_domain),
      recipient: v.recipient,
      amount: String(v.amount),
      maxFee: v.max_fee,
    }),
  ],
};

export function encodeHubMsg(typeUrl: string, value: any): Uint8Array {
  const encode = HUB_ENCODERS[typeUrl];
  if (!encode) throw new Error(`no encoder for ${typeUrl}`);
  return new Uint8Array(encode(value));
}

const registry = new Registry();
for (const typeUrl of Object.keys(HUB_ENCODERS)) {
  registry.register(typeUrl, {
    encode: (value: any) => ({ finish: () => encodeHubMsg(typeUrl, value) }),
    decode: () => {
      throw new Error(`decoding ${typeUrl} is not needed here`);
    },
    fromPartial: (value: any) => value,
  } as any);
}
// The bridge form builds this one from camelCase fields.
registry.register(MSG_REMOTE_TRANSFER, {
  encode: (value: any) => ({
    finish: () =>
      "tokenId" in value ? encodeRemoteTransfer(value) : encodeHubMsg(MSG_REMOTE_TRANSFER, value),
  }),
  decode: () => {
    throw new Error("decoding MsgRemoteTransfer is not needed here");
  },
  fromPartial: (value: any) => value,
} as any);

/// Sign and broadcast hub messages from the trade API in one transaction with Keplr.
/// Returns the transaction hash and its events.
export async function sendHubMsgs(opts: {
  chain: CosmosChain;
  sender: string;
  msgs: { typeUrl: string; value: any }[];
}): Promise<{ hash: string; events: readonly any[] }> {
  if (!window.keplr) throw new Error("Keplr is not installed");
  const signer = window.getOfflineSigner!(opts.chain.chainId);
  const client = await SigningStargateClient.connectWithSigner(opts.chain.rpc, signer, { registry });
  // Generous rather than simulated: a launch's one transaction carries up to a dozen messages,
  // and unused gas costs nothing beyond the fee at the chain's minimum price.
  const gas = 300_000 + 250_000 * opts.msgs.length;
  const fee = { amount: [{ denom: opts.chain.denom, amount: String(Math.ceil(gas * 0.004)) }], gas: String(gas) };
  const result = await client.signAndBroadcast(opts.sender, opts.msgs, fee);
  if (result.code !== 0) throw new Error(result.rawLog || `transaction failed (${result.code})`);
  return { hash: result.transactionHash, events: result.events };
}

/// Every attribute `key` of events whose type ends with `suffix`, unquoted.
export function eventValues(events: readonly any[], suffix: string, key: string): string[] {
  return events
    .filter((e) => String(e.type).endsWith(suffix))
    .flatMap((e) => (e.attributes ?? []).filter((a: any) => a.key === key))
    .map((a: any) => String(a.value).replace(/^"|"$/g, ""));
}

/// What a sender offers the paymaster for a quote: the module charges only the quote.
export function maxFeeFor(quoted: bigint): bigint {
  return feeCeiling(quoted);
}

/** Send a warp transfer from Celestia and return the transaction hash. */
export async function sendFromCelestia(opts: {
  chain: CosmosChain;
  tokenId: string;
  token: TokenId;
  destinationDomain: number;
  recipient: string;
  amount: bigint;
  sender: string;
  /// The live quote, so the ceiling is set against what delivery actually costs today.
  quotedFee: bigint;
}): Promise<string> {
  if (!window.keplr) throw new Error("Keplr is not installed");
  const signer = window.getOfflineSigner!(opts.chain.chainId);

  const client = await SigningStargateClient.connectWithSigner(opts.chain.rpc, signer, {
    registry,
  });

  const message = {
    typeUrl: MSG_REMOTE_TRANSFER,
    value: {
      sender: opts.sender,
      tokenId: opts.tokenId,
      destinationDomain: opts.destinationDomain,
      recipient: opts.recipient,
      amount: opts.amount.toString(),
      // The IGP quote is paid out of this, so it has to cover the destination's gas. The
      // relayer refunds nothing, so quoting high only costs the sender.
      maxFee: {
        denom: opts.chain.denom,
        amount: feeCeiling(opts.quotedFee).toString(),
      },
    } satisfies RemoteTransfer,
  };

  const fee = {
    amount: [{ denom: opts.chain.denom, amount: "5000" }],
    gas: "400000",
  };

  const result = await client.signAndBroadcast(opts.sender, [message], fee);
  if (result.code !== 0) throw new Error(result.rawLog || `transaction failed (${result.code})`);
  return result.transactionHash;
}

/** The Hyperlane message id a Celestia transfer produced, read back from its events. */
export async function messageIdFromCelestiaTx(
  chain: CosmosChain,
  txHash: string,
): Promise<string | null> {
  const response = await fetch(`${chain.rest}/cosmos/tx/v1beta1/txs/${txHash}`);
  if (!response.ok) return null;
  const body = await response.json();
  for (const event of body?.tx_response?.events ?? []) {
    if (!event.type.endsWith("EventInsertedIntoTree")) continue;
    for (const attribute of event.attributes ?? []) {
      if (attribute.key === "message_id") {
        // Cosmos event values are JSON-quoted strings.
        return attribute.value.replace(/^"|"$/g, "");
      }
    }
  }
  return null;
}

/// What a sender is willing to pay the IGP, as a multiple of the live quote.
///
/// A ceiling, not the price: the module charges the quote and this only bounds it. It has to
/// leave real headroom, because the oracle repushes hourly and gas can move between the quote
/// the page showed and the block the transfer lands in. A fixed number is the wrong shape -
/// 2 TIA looked generous against a 1.27 TIA Sepolia quote until gas rose.
const FEE_CEILING_MULTIPLE = 4n;

/// The smallest ceiling worth sending, whatever the quote says.
///
/// A multiple of zero is zero, and the warp module rejects a transfer whose max fee is zero
/// with "maxFee is required". Eden quotes zero honestly: gas there costs 0.01 gwei and the
/// rounding into utia takes the rest, so a destination that is genuinely almost free was the
/// one destination the page could not send to. The module still charges only the quote, so a
/// floor costs the sender nothing.
const MIN_FEE_CEILING = 1_000_000n;

function feeCeiling(quoted: bigint): bigint {
  const ceiling = quoted * FEE_CEILING_MULTIPLE;
  return ceiling > MIN_FEE_CEILING ? ceiling : MIN_FEE_CEILING;
}

export { DECIMALS };
