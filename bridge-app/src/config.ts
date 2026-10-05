// Every address the UI needs, in one place. Deployment values live here and nowhere else,
// so pointing the app at a different deployment is a single-file change.

/// Absolute URL for a path this app's own server proxies.
///
/// It has to be absolute rather than a bare path. CosmJS decides between HTTP and WebSocket
/// by looking for an `http://` or `https://` prefix, and anything else is assumed to be a
/// socket - a relative path fails with "Base URL is missing a protocol", which says nothing
/// about the real cause. `fetch` is happy either way, so only the RPC needs this.
const sameOrigin = (path: string): string =>
  typeof window === "undefined" ? path : `${window.location.origin}${path}`;

/// A deployment value that the local devnet overrides.
///
/// `make start` writes these into .env.local from what `make init` actually deployed, so the
/// devnet points at a chain whose ids are minted at genesis and cannot be known in advance.
/// Unset, every one of them falls back to the live testnet value below.
const env = (key: string, fallback: string): string =>
  (import.meta.env[key] as string | undefined) ?? fallback;

const envNum = (key: string, fallback: number): number => {
  const raw = import.meta.env[key] as string | undefined;
  const parsed = raw === undefined ? NaN : Number(raw);
  return Number.isFinite(parsed) ? parsed : fallback;
};

export type ChainId = "sepolia" | "arbitrum" | "base" | "eden" | "celestia";
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
  /** Gas token, for the network MetaMask may have to be told about. ETH where absent. */
  nativeCurrency?: { name: string; symbol: string; decimals: number };
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
    // Proxied by the server that serves this app, so the browser never depends on a
    // rate-limited public endpoint and the upstream key stays server side.
    rpc: env("VITE_SEPOLIA_RPC", "https://ethereum-sepolia-rpc.publicnode.com"),
    // Blockscout rather than Etherscan, on every EVM chain. `TeeDcapIsm` is verified there and
    // not on Etherscan, which indexes the same contract but holds no source for it, so an
    // Etherscan link lands on raw bytecode. Verifying both would need an Etherscan API key.
    explorer: "https://eth-sepolia.blockscout.com",
    mailbox: "0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766",
    ism: env("VITE_SEPOLIA_ISM", "0x83B41448ADfBdde1926575f774489892F88190A8") as `0x${string}`,
  },
  arbitrum: {
    kind: "evm",
    id: "arbitrum",
    name: "Arbitrum Sepolia",
    domain: 421614,
    chainIdHex: "0x66eee",
    rpc: env("VITE_ARBITRUM_RPC", "https://arbitrum-sepolia-rpc.publicnode.com"),
    explorer: "https://arbitrum-sepolia.blockscout.com",
    mailbox: "0x598facE78a4302f11E3de0bee1894Da0b2Cb71F8",
    ism: env("VITE_ARBITRUM_ISM", "0xD50322542cCA994322760f170D3A2df8d7f5817e") as `0x${string}`,
  },
  base: {
    kind: "evm",
    id: "base",
    name: "Base Sepolia",
    domain: 84532,
    chainIdHex: "0x14a34",
    rpc: env("VITE_BASE_RPC", "https://base-sepolia-rpc.publicnode.com"),
    explorer: "https://base-sepolia.blockscout.com",
    mailbox: "0x6966b0E55883d49BFB24539356a2f8A673E02039",
    ism: env("VITE_BASE_ISM", "0xcF5929abd3Baa03BB161745319C9E2d2Ce1201C4") as `0x${string}`,
  },
  eden: {
    kind: "evm",
    id: "eden",
    name: "Eden",
    domain: 3735928814,
    chainIdHex: "0xdeadbfee",
    rpc: env("VITE_EDEN_RPC", "https://rpc.testnet.eden.gateway.fm/"),
    explorer: "https://eden-testnet.blockscout.com",
    // Ours, unlike the other three. Eden had no Hyperlane deployment, so the mailbox and the
    // merkle tree hook were deployed with the rest of this bridge.
    mailbox: "0x1D32350f3440BEa7f7E450Aa085f63E0d7E38729",
    ism: env("VITE_EDEN_ISM", "0x84D9b9223609CEd908f247DD88f9Ae306a768E2e") as `0x${string}`,
    // Eden pays gas in TIA at 18 decimals, not in ETH. The synthetic TIA this bridge mints
    // here is a separate ERC20 at 6 decimals, matching the collateral on Celestia.
    nativeCurrency: { name: "TIA", symbol: "TIA", decimals: 18 },
  },
  celestia: {
    kind: "cosmos",
    id: "celestia",
    name: env("VITE_CELESTIA_NAME", "Celestia Mocha"),
    domain: envNum("VITE_CELESTIA_DOMAIN", 1297040200),
    chainId: env("VITE_CELESTIA_CHAIN_ID", "mocha-5"),
    // Same-origin by default, both of them, and for the same reason in two flavours.
    //
    // Mocha's public REST sends no CORS header at all. Its RPC is worse: it answers the
    // preflight with `Access-Control-Allow-Origin: *` and then omits that header from the
    // POST, so the browser waves the preflight through and refuses to read the reply. That
    // reaches the user as "Failed to fetch" at the moment they press Bridge, with nothing in
    // the console pointing at the cause. Both are proxied by the server that serves this app.
    rpc: import.meta.env.VITE_CELESTIA_RPC ?? sameOrigin("/celestia-rpc"),
    rest: import.meta.env.VITE_CELESTIA_REST ?? "/celestia",
    explorer: env("VITE_CELESTIA_EXPLORER", "https://mocha.celenium.io"),
    bech32Prefix: "celestia",
    denom: "utia",
    mailboxId: env(
      "VITE_CELESTIA_MAILBOX_ID",
      "0x68797065726c616e650000000000000000000000000000000000000000000000",
    ),
    /// The paymaster a Celestia-origin transfer pays, quoted live before sending.
    ///
    /// The devnet has no paymaster: its mailbox has no required hook, so dispatch is free and
    /// there is nothing to quote.
    igpId: env(
      "VITE_CELESTIA_IGP_ID",
      "0x726f757465725f706f73745f6469737061746368000000040000000000000002",
    ),
    ismId: env(
      "VITE_CELESTIA_ISM_ID",
      "0x726f757465725f69736d00000000000000000000000000010000000000000011",
    ),
  },
};

/** A warp router, per chain, per token. `null` where the route is not deployed yet. */
export const ROUTERS: Record<TokenId, Partial<Record<ChainId, string>>> = {
  TIA: {
    celestia: env(
      "VITE_CELESTIA_TIA_ROUTER",
      "0x726f757465725f61707000000000000000000000000000010000000000000000",
    ),
    sepolia: env("VITE_SEPOLIA_TIA_ROUTER", "0x9822eE81C82138F88D759faef1AC168aDfEe1467"),
    arbitrum: env("VITE_ARBITRUM_TIA_ROUTER", "0x41f992F671D04c5C26350E64FFA3E1D90bc33bcB"),
    base: env("VITE_BASE_TIA_ROUTER", "0xF50470146B36c638b981e437AB37DfEd9a02FAb3"),
    eden: env("VITE_EDEN_TIA_ROUTER", "0xD2babc9BE1055551b7AB98c440222862a1646158"),
  },
  // A deployment that has not created these leaves them unset, and the route reports itself
  // as not deployed rather than offering a Bridge button that cannot work. The addresses are
  // the previous deployment's; they are defaults, not a claim that this one has them.
  USDC: {
    celestia: env("VITE_CELESTIA_USDC_ROUTER", "0x726f757465725f61707000000000000000000000000000020000000000000001"),
    sepolia: env("VITE_SEPOLIA_USDC_ROUTER", "0xfb611B6f6CE92033960e99C2D65cee4237e64cDD"),
    arbitrum: env("VITE_ARBITRUM_USDC_ROUTER", "0x8C87fd144006C651430450df8b61A15EeB3FF436"),
    base: env("VITE_BASE_USDC_ROUTER", "0x285b590ee43A1374AA131e7390D0CA687Be43DF9"),
    eden: env("VITE_EDEN_USDC_ROUTER", "0xc09fbf8F17E96ce746D39f9d11a9dD1813F2d220"),
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

/// What happens between the origin finalising and the funds arriving.
///
/// Nothing is proved any more: the destination verifies the enclave's quote itself, so this
/// is one attestation plus one transaction. Measured end to end on this deployment, not
/// estimated: 16s and 23s for Celestia to Arbitrum. The default leaves room for a relayer
/// tick and a slow destination block.
export const PROVING_SECONDS = envNum("VITE_PROVING_SECONDS", 45);

/// How long each origin takes to reach the finality the enclave will attest, and why.
///
/// Arbitrum, Base and Eden are attested from a block their sequencer signed, so they are as
/// quick as the chains with fast finality.
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
    seconds: 30,
    reason:
      "Arbitrum's sequencer signs every block it produces, and the enclave checks that " +
      "signature before it attests the root.",
  },
  base: {
    seconds: 30,
    reason:
      "Base's sequencer signs every block it produces, and the enclave checks that " +
      "signature before it attests the root.",
  },
  eden: {
    // Eden's sequencer batches roughly every eleventh block into one Celestia blob, and the
    // enclave will not read a header the light client has not seen. Measured end to end at
    // about ninety seconds; the margin covers a slow blob.
    seconds: 3 * 60,
    reason:
      "Eden has no consensus of its own. Its sequencer publishes each signed header to " +
      "Celestia, and the enclave waits for that blob before it will attest the root.",
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
  // An empty string means "this deployment does not have it", which is how a route is turned
  // off from configuration without shipping a different build.
  const router = ROUTERS[token][chain];
  return router ? router : null;
}

/// Celestia is the hub: every route has it on one side. No EVM chain's ISM trusts another
/// EVM chain, so an EVM-to-EVM pair is not a route even when both routers exist.
export function routeIsLive(token: TokenId, from: ChainId, to: ChainId): boolean {
  return whyNotLive(token, from, to) === null;
}

/// Why this pair cannot be bridged, or null if it can.
///
/// Worth separating, because the two reasons are nothing alike and one message for both
/// blamed the wrong thing. An EVM to EVM pair is not a route for *any* token, so saying the
/// token is not deployed sends someone looking for a missing deployment that was never meant
/// to exist. A missing router really is a missing deployment, and names the chain it is
/// missing on.
export function whyNotLive(token: TokenId, from: ChainId, to: ChainId): string | null {
  if (from === to) return "Pick two different chains.";
  if (from !== "celestia" && to !== "celestia") {
    return `Every route goes through ${CHAINS.celestia.name}, so ${CHAINS[from].name} to ${CHAINS[to].name} is two transfers rather than one. Bridge to ${CHAINS.celestia.name} first.`;
  }
  for (const c of [from, to]) {
    if (routerFor(token, c) === null) return `${token} is not deployed on ${CHAINS[c].name} yet.`;
  }
  return null;
}
