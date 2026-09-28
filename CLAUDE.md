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
- two tokens: TIA (Celestia collateral, synthetic on EVM) and USDC (Sepolia collateral,
  synthetic elsewhere)
- three enclaves, **three images**: `celestia`, `ethereum` and `evolve`, one per origin
  family, so a change to one origin does not re-deploy the other families' ISMs

## Where the answers already are

| file | covers |
|---|---|
| `README.md` | every live id and identity, the design, the trust model |
| `deploy/DEPLOY.md` | standing up a whole bridge, step by step; adding an asset or a chain |
| `deploy/MAINTAIN.md` | the monthly job, rolling out new code, waiting vs stuck, symptoms |
| `deploy/INTERACT.md` | wallets, sending, checking arrival, latency and cost |
| `deploy/coprocessor.toml.example` | the deployed chains and routes, verbatim but for the one key |
| `deploy/images.lock` | which Nix build each pinned image came from; `30-enclave-up.sh` refuses to deploy when it and the code disagree |
| `deploy/verify-digest.sh` | compose file to `compose_hash` to `mr_config_id`, against the signed quote |
| `deploy/check-secrets.sh` | run before every commit |

## Secrets

`keys/` and `devnet/.state/` are gitignored and hold real credentials (Phala API token, Sepolia
key, mnemonics). Never commit them, never copy them into a tracked file, never send them to an
external service. `deploy/check-secrets.sh` must pass before any commit.

Only the Base route uses a metered RPC: an Alchemy key (`ALCHEMY_BASE_KEY` in `devnet/.env`, or
`devnet/.state/alchemy-base-key` on older hosts), on ark wired to the single `rpc` field of `[chains.base]`. It is a free tier, so archive
`eth_getProof` works but `eth_getLogs` is capped at 10 blocks; Base logs therefore go to
`sepolia.base.org`. Every other endpoint is a free public one.

## Gotchas worth knowing before debugging

- **L2 origin routes are slow by design.** `base-to-celestia` and `arbitrum-to-celestia` derive
  their root from the L2's dispute anchor on L1, so a transfer cannot land until the dispute game
  covering its block resolves. On Base Sepolia that is exactly 5 days plus about 3 minutes, and
  the anchor adopts a game the moment it resolves. A transfer sitting for days is normal, not a
  stall. Check `AnchorStateRegistry.getAnchorRoot()` against the dispatch block before assuming
  anything is broken.
- **We reuse the canonical Hyperlane deployments on the EVM chains**, so their merkle tree hooks
  carry other people's traffic. A `leaves=N` line in the relayer log is the batch size, not the
  tree size.
- **The `routers` list is a trigger filter**, affecting latency rather than delivery, because
  merkle tree replay forces batch completeness.
- **Eden's executor is ev-reth's own** (`ev-revm`, pinned to tag `v0.6.0`), not a
  reimplementation, so its precompiles, fee sink and custom transaction types come from the
  chain being verified rather than from guesswork.
- **Eden runs Osaka, not Prague.** Simple transfers execute the same under both, so the first
  fixtures passed on Prague and proved nothing; a DCAP verification does not, because P-256
  verification is an Osaka precompile. Under Prague it reverts with empty data.
- **The per-family split bounds origin-specific changes only.** `flake.nix` gives each family
  its own source filter, so editing `celestia/eden.rs` or `celestia/eden/` moves the evolve
  digest alone. `Cargo.lock` and the shared modules (`attest.rs`, `origin.rs`, `state.rs`,
  `evm.rs`) are in every image's source, so touching those still moves all three.
- **A replacement ISM resumes, it never re-anchors.** `80-evm-isms.sh` and
  `85-celestia-isms.sh` start a replacement from the old ISM's last state with only the identity
  (last 32 bytes) swapped, so nothing in flight is lost, and stop rather than fall back to the
  head. Only a first deploy anchors at the head.
- **Eden's root is re-executed, not believed.** The enclave runs the blocks that changed the
  state, from the root the ISM already trusts, and the chain has to arrive at the root the
  sequencer signed. Only state-changing blocks are sent, which is safe because a missing one
  shows up as a root mismatch. Eden does **not** burn the base fee: it pays it to the block
  beneficiary, and the executor credits that back. See `crates/tee-node/src/celestia/eden/` and the
  Design section of `README.md`.
- **Rotating the enclave identity re-points routers, never redeploys them.** Redeploying a
  collateral router abandons its escrow; that stranded real USDC on Sepolia once.

## Hard constraints

- **celestia-app**: only ever the `jonas/tee-ism` branch. `main` is critical company
  infrastructure and must never be touched.
- **No co-author or "generated with" trailers** in commits or PR descriptions. Commits are in the
  user's name.
- **No em-dashes in UI copy.**
- Ask before adding components or changing the stack.
