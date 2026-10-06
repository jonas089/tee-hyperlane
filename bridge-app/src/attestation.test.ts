import { describe, expect, it } from "vitest";
import { normaliseAttestation } from "./bridge";

describe("normaliseAttestation", () => {
  it("reads the relayer's snake_case answer", () => {
    const a = normaliseAttestation({
      height: 653374,
      state_root: "0xabc",
      quote: "04",
      measurements: { mrTd: "f0", osImageHash: "bd", composeHash: "73" },
      batch: ["0x01"],
    });
    expect(a).toEqual({
      height: 653374,
      stateRoot: "0xabc",
      quote: "04",
      measurements: { mrTd: "f0", osImageHash: "bd", composeHash: "73" },
      batch: ["0x01"],
    });
  });

  it("keeps an already camelCase attestation and survives missing fields", () => {
    expect(normaliseAttestation({ stateRoot: "0xdef" })?.stateRoot).toBe("0xdef");
    expect(normaliseAttestation({})?.batch).toEqual([]);
    expect(normaliseAttestation(null)).toBeNull();
  });
});
