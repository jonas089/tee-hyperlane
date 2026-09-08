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

const registry = new Registry();
registry.register(MSG_REMOTE_TRANSFER, {
  encode: (value: RemoteTransfer) => ({
    finish: () => encodeRemoteTransfer(value),
  }),
  decode: () => {
    throw new Error("decoding MsgRemoteTransfer is not needed here");
  },
  fromPartial: (value: RemoteTransfer) => value,
} as any);

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
        amount: (opts.quotedFee * FEE_CEILING_MULTIPLE).toString(),
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

export { DECIMALS };
