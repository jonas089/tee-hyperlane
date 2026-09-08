// Every address the UI needs, in one place. Deployment values live here and nowhere else,
// so pointing the app at a different deployment is a single-file change.

export type ChainId = "sepolia" | "arbitrum" | "base" | "celestia";
export type TokenId = "TIA" | "USDC";

export interface EvmChain {
  kind: "evm";
  id: ChainId;
  name: string;
  domain: number;
  chainIdHex: string;
  rpc: string;
  explorer: string;
  mailbox: `0x${string}`;
  /** The ISM that authorises messages arriving here. */
  ism: `0x${string}` | null;
}

export interface CosmosChain {
  kind: "cosmos";
  id: ChainId;
  name: string;
  domain: number;
  chainId: string;
  rpc: string;
  rest: string;
  explorer: string;
  bech32Prefix: string;
  denom: string;
  mailboxId: string;
  igpId: string;
  ismId: string;
}

export type Chain = EvmChain | CosmosChain;

export const CHAINS: Record<ChainId, Chain> = {
  sepolia: {
    kind: "evm",
    id: "sepolia",
    name: "Ethereum Sepolia",
    domain: 11155111,
    chainIdHex: "0xaa36a7",
    rpc: "https://ethereum-sepolia-rpc.publicnode.com",
    explorer: "https://sepolia.etherscan.io",
    mailbox: "0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766",
    ism: "0x6f31D79D898f86a60832Fd1caB31ceC67Bc71Fb6",
  },
  arbitrum: {
    kind: "evm",
    id: "arbitrum",
    name: "Arbitrum Sepolia",
    domain: 421614,
    chainIdHex: "0x66eee",
    rpc: "https://arbitrum-sepolia-rpc.publicnode.com",
    explorer: "https://sepolia.arbiscan.io",
    mailbox: "0x598facE78a4302f11E3de0bee1894Da0b2Cb71F8",
    ism: "0x21bdf13D66D3e5F0D4793B64bb4c85034B9EDc88",
  },
  base: {
    kind: "evm",
    id: "base",
    name: "Base Sepolia",
    domain: 84532,
    chainIdHex: "0x14a34",
    rpc: "https://base-sepolia-rpc.publicnode.com",
    explorer: "https://sepolia.basescan.org",
    mailbox: "0x6966b0E55883d49BFB24539356a2f8A673E02039",
    ism: "0x1D32350f3440BEa7f7E450Aa085f63E0d7E38729",
  },
  celestia: {
    kind: "cosmos",
    id: "celestia",
    name: "Celestia Mocha",
    domain: 1297040200,
    chainId: "mocha-5",
    rpc: "https://rpc-mocha.pops.one",
    // Same-origin by default: Mocha's public REST sends no CORS header, so the browser
    // cannot read it directly. nginx proxies /celestia to it.
    rest: import.meta.env.VITE_CELESTIA_REST ?? "/celestia",
    explorer: "https://mocha.celenium.io",
    bech32Prefix: "celestia",
    denom: "utia",
    mailboxId: "0x68797065726c616e650000000000000000000000000000000000000000000000",
    /// The paymaster a Celestia-origin transfer pays, quoted live before sending.
    igpId: "0x726f757465725f706f73745f6469737061746368000000040000000000000002",
    ismId: "0x726f757465725f69736d0000000000000000000000000001000000000000000c",
  },
};

/** A warp router, per chain, per token. `null` where the route is not deployed yet. */
export const ROUTERS: Record<TokenId, Partial<Record<ChainId, string>>> = {
  TIA: {
    celestia: "0x726f757465725f61707000000000000000000000000000010000000000000000",
    sepolia: "0xFeA14C1444A7a8beAb7122fdE5A168212D7185bE",
    arbitrum: "0xFeA14C1444A7a8beAb7122fdE5A168212D7185bE",
    base: "0xf4197C55C944987E9b10e09C0A47915211769B78",
  },
  USDC: {
    celestia: "0x726f757465725f61707000000000000000000000000000020000000000000001",
    sepolia: "0xfb611B6f6CE92033960e99C2D65cee4237e64cDD",
    arbitrum: "0xb9E5E3eb926EA22B951d2fb7392F9F3D6c704054",
    base: "0x0ee6374a92ba4E11F920A23c6dd271b594D69A9B",
  },
};

export const DECIMALS: Record<TokenId, number> = { TIA: 6, USDC: 6 };

/// Gas each warp router is enrolled with, and therefore what the paymaster quotes against.
export const REMOTE_ROUTER_GAS = 50000;

/// What each token is called in Celestia's bank module. On the EVM side the router *is* the
/// ERC20, so its address is enough and there is nothing to name here.
export const CELESTIA_DENOM: Record<TokenId, string> = {
  TIA: "utia",
  USDC: "hyperlane/0x726f757465725f61707000000000000000000000000000020000000000000001",
};

/// Two Groth16 proofs on the coprocessor's CPU, plus the wait for a free prover.
///
/// Measured on the machine actually running this, not estimated: 2857s and 2592s for the two
/// proofs of one batch. Routes share a single prover, so a transfer can also queue behind
/// another route's batch, which is why this is not simply the sum.
export const PROVING_SECONDS = 100 * 60;

/// How long each origin takes to reach the finality the enclave will attest, and why.
///
/// Arbitrum and Base are both optimistic rollups, so both wait out a challenge window before
/// their state root is trustless. They differ only in how long that window is configured to
/// be - which is the difference between half an hour and most of a week.
export interface OriginFinality {
  seconds: number;
  /// Shown to the sender before they commit to a transfer they cannot speed up.
  reason: string;
}

export const ORIGIN_FINALITY: Record<ChainId, OriginFinality> = {
  celestia: {
    seconds: 60,
    reason: "Celestia finalises in a single block.",
  },
  sepolia: {
    seconds: 13 * 60,
    reason: "Ethereum is attested at its finalized head, roughly two epochs behind.",
  },
  arbitrum: {
    seconds: 35 * 60,
    reason:
      "Arbitrum is an optimistic rollup: its state root is only trustless once Ethereum " +
      "confirms the assertion, about every half hour on Sepolia.",
  },
  base: {
    // Measured on Base Sepolia: the game the anchor points at was created 120.1 hours before
    // it resolved, and the portal adds no further delay.
    seconds: 120 * 60 * 60,
    reason:
      "Base is an optimistic rollup. Its root only becomes final once a dispute game has " +
      "run its full challenge clock, which on Sepolia takes five days.",
  },
};

/// Total expected wait for a transfer leaving this chain.
export function expectedSeconds(from: ChainId): number {
  return ORIGIN_FINALITY[from].seconds + PROVING_SECONDS;
}

/// A wait long enough that a sender should be told before they commit, not after.
export const SLOW_ORIGIN_SECONDS = 60 * 60;

/** Where the coprocessor publishes attestations. Same origin in production. */
export const RELAYER_API = import.meta.env.VITE_RELAYER_API ?? "/api";

export function routerFor(token: TokenId, chain: ChainId): string | null {
  return ROUTERS[token][chain] ?? null;
}

/// Celestia is the hub: every route has it on one side. No EVM chain's ISM trusts another
/// EVM chain, so an EVM-to-EVM pair is not a route even when both routers exist.
export function routeIsLive(token: TokenId, from: ChainId, to: ChainId): boolean {
  if (from === to) return false;
  if (from !== "celestia" && to !== "celestia") return false;
  return routerFor(token, from) !== null && routerFor(token, to) !== null;
}
