import { describe, expect, it } from "vitest";
import { expectedPathSeconds, expectedSeconds, legsFor, whyNotLive } from "./config";

describe("legsFor", () => {
  it("is one leg when Celestia is on either side", () => {
    expect(legsFor("celestia", "base")).toEqual([{ from: "celestia", to: "base" }]);
    expect(legsFor("eden", "celestia")).toEqual([{ from: "eden", to: "celestia" }]);
  });

  it("goes through Celestia between two other chains", () => {
    expect(legsFor("sepolia", "base")).toEqual([
      { from: "sepolia", to: "celestia" },
      { from: "celestia", to: "base" },
    ]);
  });
});

describe("whyNotLive", () => {
  it("accepts a two-leg path and refuses the same chain twice", () => {
    expect(whyNotLive("TIA", "sepolia", "base")).toBeNull();
    expect(whyNotLive("TIA", "base", "base")).toBe("Pick two different chains.");
  });
});

describe("expectedPathSeconds", () => {
  it("adds up both legs", () => {
    expect(expectedPathSeconds("sepolia", "eden")).toBe(
      expectedSeconds("sepolia") + expectedSeconds("celestia"),
    );
    expect(expectedPathSeconds("celestia", "eden")).toBe(expectedSeconds("celestia"));
  });
});
