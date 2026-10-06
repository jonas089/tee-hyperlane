// Encoders for the hub messages the trade API returns, so a mnemonic can sign them. The same
// field numbers as bridge-app/src/celestia.ts, from the hyperlane-cosmos protos; keep the two
// in step.

import { Registry } from "@cosmjs/proto-signing";

function varint(value) {
  const out = [];
  let n = value;
  while (n > 0x7f) {
    out.push((n & 0x7f) | 0x80);
    n >>>= 7;
  }
  out.push(n);
  return out;
}
const tag = (field, wire) => varint((field << 3) | wire);
const bytesField = (field, bytes) => [...tag(field, 2), ...varint(bytes.length), ...bytes];
const str = (field, v) => (v ? bytesField(field, [...new TextEncoder().encode(String(v))]) : []);
const uint = (field, v) => (v ? [...tag(field, 0), ...varint(Number(v))] : []);
const bool = (field, v) => (v ? [...tag(field, 0), 1] : []);

const ENCODERS = {
  "/hyperlane.warp.v1.MsgCreateSyntheticToken": (v) => [...str(1, v.owner), ...str(2, v.origin_mailbox)],
  "/hyperlane.warp.v1.MsgSetToken": (v) => [
    ...str(1, v.owner),
    ...str(2, v.token_id),
    ...str(3, v.new_owner),
    ...str(4, v.ism_id),
    ...bool(7, v.renounce_ownership),
  ],
  "/hyperlane.warp.v1.MsgEnrollRemoteRouter": (v) => [
    ...str(1, v.owner),
    ...str(2, v.token_id),
    ...bytesField(3, [
      ...uint(1, v.remote_router.receiver_domain),
      ...str(2, v.remote_router.receiver_contract),
      ...str(3, v.remote_router.gas),
    ]),
  ],
  "/hyperlane.warp.v1.MsgUnrollRemoteRouter": (v) => [...str(1, v.owner), ...str(2, v.token_id), ...uint(3, v.receiver_domain)],
  "/hyperlane.core.v1.MsgProcessMessage": (v) => [
    ...str(1, v.mailbox_id),
    ...str(2, v.relayer),
    ...str(3, v.metadata),
    ...str(4, v.message),
  ],
  "/hyperlane.warp.v1.MsgRemoteTransfer": (v) => [
    ...str(1, v.sender),
    ...str(2, v.token_id),
    ...uint(3, v.destination_domain),
    ...str(4, v.recipient),
    ...str(5, v.amount),
    ...str(7, "0"),
    ...bytesField(8, [...str(1, v.max_fee.denom), ...str(2, v.max_fee.amount)]),
  ],
};

export const registry = new Registry();
for (const [typeUrl, encode] of Object.entries(ENCODERS)) {
  registry.register(typeUrl, {
    encode: (value) => ({ finish: () => new Uint8Array(encode(value)) }),
    decode: () => ({}),
    fromPartial: (value) => value,
  });
}

/// Every attribute `key` of events whose type ends with `suffix`, unquoted.
export function eventValues(events, suffix, key) {
  return events
    .filter((e) => String(e.type).endsWith(suffix))
    .flatMap((e) => (e.attributes ?? []).filter((a) => a.key === key))
    .map((a) => String(a.value).replace(/^"|"$/g, ""));
}
