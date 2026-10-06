import { describe, expect, it } from "vitest";
import { isNonceTooLow } from "./bridge";

describe("isNonceTooLow", () => {
  it("matches the node's rejection however MetaMask wraps it", () => {
    const infura = {
      code: -32603,
      message: "Internal JSON-RPC error.",
      data: { message: "RPC 0xaa36a7 Infura eth_sendRawTransaction: nonce too low: next nonce 1063, tx nonce 1062" },
    };
    expect(isNonceTooLow(infura)).toBe(true);
    expect(isNonceTooLow(new Error("nonce too low: next nonce 5, tx nonce 4"))).toBe(true);
  });

  it("never retries a rejection by the user, or anything else", () => {
    expect(isNonceTooLow({ code: 4001, message: "User rejected the request. nonce too low" })).toBe(false);
    expect(isNonceTooLow({ code: -32603, message: "insufficient funds for gas" })).toBe(false);
    expect(isNonceTooLow(null)).toBe(false);
  });
});
