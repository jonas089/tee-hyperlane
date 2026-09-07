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
    ism: "0xb9E5E3eb926EA22B951d2fb7392F9F3D6c704054",
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
    ism: "0xf48fefa3848f1F25093D3e7937BdD4b80B421D64",
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
    ism: "0xFeA14C1444A7a8beAb7122fdE5A168212D7185bE",
  },
  celestia: {
    kind: "cosmos",
    id: "celestia",
    name: "Celestia Mocha",
    domain: 1297040200,
    chainId: "mocha-5",
    rpc: "https://rpc-mocha.pops.one",
    rest: "https://api-mocha.pops.one",
    explorer: "https://mocha.celenium.io",
    bech32Prefix: "celestia",
    denom: "utia",
    mailboxId: "0x68797065726c616e650000000000000000000000000000000000000000000000",
    ismId: "0x726f757465725f69736d000000000000000000000000002a0000000000000001",
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

/// What each token is called in Celestia's bank module. On the EVM side the router *is* the
/// ERC20, so its address is enough and there is nothing to name here.
export const CELESTIA_DENOM: Record<TokenId, string> = {
  TIA: "utia",
  USDC: "hyperlane/0x726f757465725f61707000000000000000000000000000020000000000000001",
};

/// Roughly how long a transfer out of each origin takes, in seconds: how long the origin
/// takes to reach the finality the enclave requires, plus two Groth16 proofs on CPU. Used
/// only to show an expected arrival - the real answer is always what the destination says.
export const EXPECTED_SECONDS: Record<ChainId, number> = {
  celestia: 15 * 60,
  sepolia: 28 * 60,
  arbitrum: 45 * 60,
  base: 45 * 60,
};

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
