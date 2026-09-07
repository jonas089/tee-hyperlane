import { describe, expect, it } from "vitest";
import { encodeRemoteTransfer } from "./celestia";

/// `MsgRemoteTransfer` is encoded by hand, so the only meaningful test is against bytes the
/// chain itself produced and accepted. These are from mocha-5 transaction
/// CE37AB2DBB0C52284C664410C1B4ED411932116D4118B7CACD72CCDEB0B54A13, a 0.05 TIA transfer to
/// Base Sepolia, pulled out of its `Any.value`.
const ON_CHAIN =
  "0a2f63656c65737469613176386538337873346e6c666c7071357675657472757876766d747a326c6c323478" +
  "3568763937124230783732366637353734363537323566363137303730303030303030303030303030303030" +
  "303030303030303030303030313030303030303030303030303030303018b494052242307830303030303030" +
  "3030303030303030303030303030303030333138643232666161316530663239656163376566363434613866" +
  "616336373666363638386431652a0535303030303a013042090a0475746961120130";

const hex = (bytes: Uint8Array) =>
  [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");

describe("encodeRemoteTransfer", () => {
  it("reproduces a transfer the chain accepted, byte for byte", () => {
    const encoded = encodeRemoteTransfer({
      sender: "celestia1v8e83xs4nlflpq5vuetruxvvmtz2ll24x5hv97",
      tokenId: "0x726f757465725f61707000000000000000000000000000010000000000000000",
      destinationDomain: 84532,
      recipient: "0x000000000000000000000000318d22faa1e0f29eac7ef644a8fac676f6688d1e",
      amount: "50000",
      maxFee: { denom: "utia", amount: "0" },
    });
    expect(hex(encoded)).toBe(ON_CHAIN);
  });

  it("encodes the destination domain as a varint, not a string", () => {
    // 84532 is three varint bytes; a string would be five ASCII ones behind a length prefix.
    const encoded = encodeRemoteTransfer({
      sender: "a",
      tokenId: "b",
      destinationDomain: 84532,
      recipient: "c",
      amount: "1",
      maxFee: { denom: "utia", amount: "0" },
    });
    expect(hex(encoded)).toContain("18b49405");
  });
});
