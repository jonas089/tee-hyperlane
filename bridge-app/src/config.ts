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
/// TIA, teeUSD, or a launched token's hub id: launched tokens join at runtime through
/// `registerToken`, from the trade API's catalog.
export type TokenId = "TIA" | "teeUSD" | (string & {});

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
export const ROUTERS: Record<string, Partial<Record<ChainId, string>>> = {
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
  // Ours: a fixed supply minted on Celestia, synthetic everywhere else. A deployment that has
  // not created it leaves these unset, and the route reports itself as not deployed.
  teeUSD: {
    celestia: env("VITE_CELESTIA_TEEUSD_ROUTER", "0x726f757465725f61707000000000000000000000000000020000000000000002"),
    sepolia: env("VITE_SEPOLIA_TEEUSD_ROUTER", "0x1be2055d49c0a350C137605a62861f58Aa750297"),
    arbitrum: env("VITE_ARBITRUM_TEEUSD_ROUTER", "0xF699EB10087A1e01993ed628C198fe6e231830eD"),
    base: env("VITE_BASE_TEEUSD_ROUTER", "0xa8cc6ec4855b1415c03f3aE51953a3643f7991a5"),
    eden: env("VITE_EDEN_TEEUSD_ROUTER", "0xa4697140E90F39D94F3C64D40175FA6F207bc8EC"),
  },
};

export const DECIMALS: Record<string, number> = { TIA: 6, teeUSD: 6 };

/// Gas each warp router is enrolled with, and therefore what the paymaster quotes against.
export const REMOTE_ROUTER_GAS = 50000;

/// What each token is called in Celestia's bank module. On the EVM side the router *is* the
/// ERC20, so its address is enough and there is nothing to name here.
export const CELESTIA_DENOM: Record<string, string> = {
  TIA: "utia",
  // A synthetic's bank denom is its token id under `hyperlane/`.
  teeUSD: `hyperlane/${ROUTERS.teeUSD.celestia}`,
};

/// What each token is shown as. A launched token's id is its hub token id, so it needs one.
const LABELS: Record<string, string> = {};

export const labelOf = (token: string): string => LABELS[token] ?? token;

/// Add a launched token, so the bridge can carry it like TIA and teeUSD.
export function registerToken(id: string, symbol: string, routers: Partial<Record<ChainId, string>>, denom: string) {
  ROUTERS[id] = routers;
  CELESTIA_DENOM[id] = denom;
  DECIMALS[id] = 6;
  LABELS[id] = symbol;
}

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
    if (routerFor(token, c) === null) return `${labelOf(token)} is not deployed on ${CHAINS[c].name} yet.`;
  }
  return null;
}
