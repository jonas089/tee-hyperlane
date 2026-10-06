# CLAUDE.md

## What this repo is

A Hyperlane bridge between Celestia and three EVM testnets where messages are authorised by a
light client running inside a TDX enclave. The destination verifies the enclave's TDX quote
directly. There is no zero-knowledge proof anywhere in this path. `README.md` has the
architecture.

**The SP1/Groth16 stack that used to sit at this path is gone.** It survives only in `main`'s
history on GitHub (`jonas089/tee-hyperlane`, commit `e2272a0`). If you find yourself reading about
`tee-circuit/programs/state-transition`, SP1 vkeys, or a Groth16 verifier contract, you are
looking at the old stack, not at what runs. The two differ in a way that matters: the old one
ran DCAP inside the circuit, so nothing expired on chain; this one verifies DCAP on chain via
Automata PCCS on each EVM chain, so EVM-destination routes depend on collateral that goes stale.
Celestia-destination routes carry fresh collateral in the transaction and do not.

## Branch

`main` is the live branch; ark runs it. Work on a branch and merge by PR. Do not merge or push
to `main` without asking.

## What is deployed

Server "ark", Zurich, `chef@178.199.12.26`, checkout at `~/tee-ism-nonzk`, a git clone of `main`
over HTTPS. Update it with `git pull`; `devnet/.env` and `devnet/.state/` there are gitignored.

- systemd: `teeism-relayer` (every route, plus the dashboard and API on :3001) and
  `teeism-gas-oracle`. The gateway container serves the built `bridge-app/dist`. Until the
  next rollout ark also has the old `teeism-api` unit and a disabled `bridge-ui` unit, which
  the current binary cannot run; the rollout removes both.
- a local celestia-app devnet, not mocha, with pruning disabled
- a mocha light node (`mocha-light`), which only the Eden origin needs
- nginx gateway on `:3000` exposing `/rpc`, `/rest`, `/api`, `/evm/{chain}/`, `/tx/<hash>`
- eight routes: Celestia to and from each of Sepolia, Arbitrum, Base and Eden
- two tokens: TIA (Celestia collateral, synthetic on EVM) and teeUSD (ours, a fixed 1B
  minted on Celestia, synthetic everywhere), and a TIA/teeUSD Uniswap v3 pool on Sepolia,
  Base and Arbitrum. The Trade tab and `/api/v1/trade` route swaps through them
- **five images**, one per origin (`celestia`, `ethereum`, `base`, `arbitrum`, `eden`), all
  in **one shared CVM** by default, each on its own port (8080 to 8084). They share that CVM's
  identity, so a change to any image replaces all 8 ISMs. `30-enclave-up.sh --<chain>` moves a
  chain into its own CVM with its own identity. Until the rollout of the shared CVM, ark still
  runs the old three per-family CVMs (`celestia`, `ethereum`, `evolve`)

## Where the answers already are

| file | covers |
|---|---|
| `README.md` | every live id and identity, the design, the trust model |
| `deploy/DEPLOY.md` | standing up a whole bridge, step by step; adding an asset or a chain |
| `deploy/MAINTAIN.md` | the monthly job, rolling out new code, waiting vs stuck, symptoms |
| `deploy/INTERACT.md` | wallets, sending, checking arrival, latency and cost |
| `deploy/API.md` | the relayer's `/api/v1`: endpoints, transfer statuses, notification kinds, and the venue under `/api/v1/trade` |
| `mcp/` | the MCP server agents use for the venue; its docs are the Trade tab's Docs (Agents) |
| `deploy/coprocessor.toml.example` | the deployed chains and routes, verbatim but for the one key |
| `deploy/images.lock` | which Nix build each pinned image came from; `30-enclave-up.sh` refuses to deploy when it and the code disagree |
| `deploy/verify-digest.sh` | compose file to `compose_hash` to `mr_config_id`, against the signed quote |
| `deploy/check-secrets.sh` | run before every commit |

## Secrets

`keys/` and `devnet/.state/` are gitignored and hold real credentials (Phala API token, Sepolia
key, mnemonics). Never commit them, never copy them into a tracked file, never send them to an
external service. `deploy/check-secrets.sh` must pass before any commit.

Only the Base and Arbitrum routes use a metered RPC: an Alchemy key (`ALCHEMY_KEY` in
`devnet/.env`; `ALCHEMY_BASE_KEY` or `devnet/.state/alchemy-base-key` on older hosts), wired to
the `rpc` field of `[chains.base]` and `[chains.arbitrum]` and read only for the tree at the
ISM's trusted height. It is a free tier, so archive `eth_getProof` works but `eth_getLogs` is
capped at 10 blocks; logs and reads near the head go to the public endpoints. Every other
endpoint is a free public one.

## Gotchas worth knowing before debugging

- **Rollups are attested from what their sequencer signed, not re-executed.** Base: a block
  from the OP Stack p2p network, signed by `unsafeBlockSigner`; the coprocessor runs a libp2p
  listener for it (`origin/l2/base/gossip.rs`), because no RPC serves the signature. Arbitrum: a
  feed message, whose `signatureV2` covers the block hash. Eden: a header posted to mocha. The
  keys are constants in `tee-node/src/chains/l2/`; a sequencer key rotation on L1 needs a new
  image. Transfers from all three land in under a few minutes, so one waiting longer is stuck.
- **We reuse the canonical Hyperlane deployments on the EVM chains**, so their merkle tree hooks
  carry other people's traffic. A `leaves=N` line in the relayer log is the batch size, not the
  tree size.
- **The `routers` list is a trigger filter**, affecting latency rather than delivery, because
  merkle tree replay forces batch completeness.
- **v5 quotes: EVM yes, Celestia not yet.** Phala emits v4 TDX quotes today. `TeeDcapIsm`
  (`VERSION` 2) and `75-dcap-verifiers.sh` accept v5 as well, but `x/teeism` still casts to
  `QuoteV4`, so a host switch to v5 would stop the four routes into Celestia until it changes.
- **Eden runs Osaka, not Prague.** Simple transfers execute the same under both, so the first
  fixtures passed on Prague and proved nothing; a DCAP verification does not, because P-256
  verification is an Osaka precompile. Under Prague it reverts with empty data.
- **The per-chain split bounds image digests, not identities, in the shared CVM.** `flake.nix`
  gives each chain its own source filter, so editing `chains/l2/eden.rs` moves the eden digest
  alone; but the shared CVM's compose hash covers every digest, so its identity moves anyway.
  The split only saves ISMs for a chain deployed alone. `Cargo.lock` and the shared modules
  (`attest.rs`, `origin.rs`, `state.rs`, `evm.rs`) move every digest.
- **A replacement ISM resumes, it never re-anchors.** `80-evm-isms.sh` and
  `85-celestia-isms.sh` start a replacement from the old ISM's last state with only the identity
  (last 32 bytes) swapped, so nothing in flight is lost, and stop rather than fall back to the
  head. Only a first deploy anchors at the head.
- **Launched tokens are anyone's, and the relayer carries them.** A `TeeTokenFactory` per venue
  chain makes their routers and keeps ownership, so `90-evm-warp.sh` re-points them on a rotation.
  The trade API lists a token once the hub shows it renounced and wired, and adds its routers to
  `.state/proofs/launched.json`, which `worth_attesting` and the tracker accept beside `routers`.
- **Rotating the enclave identity re-points routers, never redeploys them.** Redeploying a
  router orphans what it holds or minted; that once stranded real USDC on Sepolia.

## Hard constraints

- **celestia-app**: only ever the `jonas/tee-ism` branch. `main` is critical company
  infrastructure and must never be touched.
- **No co-author or "generated with" trailers** in commits or PR descriptions. Commits are in the
  user's name.
- **No em-dashes in UI copy.**
- Ask before adding components or changing the stack.
