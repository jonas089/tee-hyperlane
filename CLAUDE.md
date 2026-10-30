# CLAUDE.md

Hyperlane bridge between Celestia and EVM testnets; messages authorised by a light client in a TDX enclave, quote verified on the destination. No ZK. Architecture: `README.md`.

## Hard constraints

- **celestia-app**: only ever the `jonas/tee-ism` branch. `main` is critical company infrastructure and must never be touched. No tags there either. The devnet runs the branch head: image `celestia-app-teeism:jonas-tee-ism`, labelled with its commit.
- **No co-author or "generated with" trailers** in commits or PR descriptions. Commits are in the user's name.
- **No em-dashes in UI copy.**
- Ask before adding components or changing the stack.
- This repo: `main` is live (ark runs it). Commit requested changes directly to `main`; never create branches. Push only when asked.
- **Never redeploy routers on an identity rotation; re-point them.** Redeploying orphans what they hold or minted (once stranded real USDC on Sepolia).

## Secrets

- `keys/` and `devnet/.state/` hold real credentials. Never commit them, copy them into tracked files, or send them to an external service.
- `teeism check-secrets` must pass before any commit.
- Only Base and Arbitrum use a metered RPC: Alchemy free tier (`ALCHEMY_KEY` in `devnet/.env`, or its older name `ALCHEMY_BASE_KEY`), in `rpc` of `[chains.base]` / `[chains.arbitrum]`, used only for the tree at the ISM's trusted height. `eth_getLogs` capped at 10 blocks. Everything else is public.

## Old stack

- SP1/Groth16 is gone; only in `main` history (`jonas089/tee-hyperlane`, `e2272a0`). `tee-circuit/programs/state-transition`, SP1 vkeys, Groth16 verifier = old, not what runs.
- Now DCAP is verified on chain via our Automata PCCS on EVM (CREATE2, salt namespace `tee-isms/v2`), so EVM-destination routes depend on collateral that goes stale (`teeism pccs seed` monthly). Celestia-destination routes carry fresh collateral and don't.

## Deployed (ark)

- `chef@178.199.12.26`, Zurich, `~/tee-ism-nonzk`, HTTPS clone of `main`. Update with `git pull`. `devnet/.env`, `devnet/.state/` gitignored.
- systemd: `teeism-relayer` (routes, dashboard + API on :3001), `teeism-gas-oracle`. Gateway serves `bridge-app/dist`.
- Local celestia-app devnet (not mocha), pruning disabled. `mocha-light` light node, Eden only.
- nginx gateway `:3000`: `/rpc`, `/rest`, `/api`, `/evm/{chain}/`, `/tx/<hash>`.
- 8 routes: Celestia to/from Sepolia, Arbitrum, Base, Eden.
- Tokens: TIA (Celestia collateral, synthetic on EVM), teeUSD (fixed 1B minted on Celestia, synthetic everywhere). Bridge only: no trading, pools or token launch.
- 5 images (`celestia`, `ethereum`, `base`, `arbitrum`, `eden`) in one shared CVM, ports 8080 to 8084. Shared identity: any image change re-pins all 8 ISMs. `teeism enclaves up --chain <chain>` gives a chain its own CVM and identity.

## Docs

| file | covers |
|---|---|
| `README.md` | every live id and identity, the design, the trust model |
| `deploy/DEPLOY.md` | standing up the bridge with `teeism`, step by step; mr_td re-pin; Intel root and collateral; adding an asset or chain |
| `cli/` | `teeism`, the only deploy tool (own Cargo workspace, so its lock never moves the enclave digests) |
| `deploy/MAINTAIN.md` | monthly job, rollouts, waiting vs stuck, symptoms |
| `deploy/ADMIN.md` | owner controls and commands, payload sources, Intel breakage |
| `deploy/INTERACT.md` | wallets, sending, arrival, latency, cost |
| `deploy/API.md` | `/api/v1` endpoints, statuses, notifications |
| `deploy/coprocessor.toml.example` | deployed chains and routes, verbatim but for the one key |
| `deploy/upstream.toml` | what `teeism upstream check` watches upstream, what we support, the manifest the judge reads |
| `deploy/images.lock` | Nix build per pinned image; `teeism enclaves up` refuses to deploy on mismatch with code |

## Gotchas

- **Rollups are attested from sequencer signatures, not re-executed.**
  - Base: OP Stack p2p block signed by `unsafeBlockSigner`; libp2p listener in `origin/l2/base/gossip.rs` (no RPC serves it).
  - Arbitrum: feed message, `signatureV2` covers the block hash.
  - Eden: header posted to mocha.
  - Keys are constants in `tee-node/src/chains/l2/`; an L1 sequencer key rotation needs a new image.
  - Transfers land within a few minutes; longer means stuck.
- EVM chains use canonical Hyperlane deployments, so hooks carry others' traffic. Relayer log `leaves=N` is batch size, not tree size.
- `routers` list is a trigger filter: affects latency, not delivery.
- **Messages prove themselves at delivery.** The enclave attests the origin tree's root and count (payload is a fixed 308 bytes); delivery metadata is `root(32) index(4 BE) proof(32*32)`, checked with Hyperlane's `branchRoot` against any root the ISM has attested (`TeeDcapIsm` `VERSION` 5, `x/teeism`). The relayer builds the branch from the tree at the trusted height plus the leaves it indexes (`branch_proof`, `hyperlane-types`). No message ids are ever on chain. A proof lands only against the root it was built for; if the ISM never attested that root (someone else attested past us, or the ISM was replaced), redelivery re-proves the parked message against the root the ISM holds (`current_proof`, `route.rs`), from the frontier kept with it plus the leaves indexed since.
- Quotes: Phala emits v4. `TeeDcapIsm` (`VERSION` 2+), the v5 verifier `teeism pccs deploy` registers, and `x/teeism` (go-tdx-guest `b0a2dd8`) accept v4 and v5. Later versions need a celestia-app upgrade.
- **dstack 0.6 only.** The enclave identity (`x/teeism` identity v3) pins mr_td, RTMR0-2 (the boot chain), and from RTMR3 `os-image-hash`, `compose-hash`, `key-provider`; no `mr-kms`, which only 0.5 recorded. `teeism enclaves up` asks for `dstack-0.6.1`. A Phala host VMM upgrade can move RTMR0 and needs a re-pin.
- **Sepolia is past Gloas; light client is ours.** From slot 11296768 the relayer sends the header RLP, enclave checks its keccak. helios is upstream `consensus-core` 0.12.0 (Gloas), a git dependency pinned by rev in `tee-hyperlane/Cargo.toml`. ark's Sepolia beacon is Lodestar (only one serving Gloas light-client data), not lighthouse.
- **Eden runs Osaka, not Prague.** Fixtures on Prague prove nothing for DCAP: P-256 verification is an Osaka precompile and reverts with empty data under Prague.
- **Shared CVM: per-chain split bounds digests, not identities.** Editing one chain's file moves only its digest, but the compose hash still moves the shared identity. `Cargo.lock` and shared modules (`attest.rs`, `origin.rs`, `state.rs`, `evm.rs`) move every digest.
- **A rotation re-pins, never re-anchors.**
  - Owner re-pins in place: `setEnclave` (EVM, revision 3+), `tx teeism update-identity` (Celestia). `teeism isms evm`, `teeism isms celestia` do this.
  - Only ISMs we don't own, or an EVM contract revision change, get a replacement: starts from the old ISM's last state, stops rather than fall back to the head.
  - Only a first deploy anchors at the head.
- **Heartbeat:** a quiet route attests daily with no new leaves (`HEARTBEAT`, `route.rs`). `TeeDcapIsm` and `x/teeism` refuse an attestation that leaves the tree root unchanged and moves the origin clock < 30 min; the enclave does not check this (it does not know the previous root).
- **Intel root CA:** only the owner can update it on Celestia (`tx teeism set-verifier-overrides --trusted-root-cert`). Nothing skips or relaxes Intel verification.
- **Ownership:** `tx teeism update-owner`. `teeism isms` re-pins only ISMs owned by the relayer (Celestia) or `EVM_SIGNER` (Ledger, or `EVM_PRIVATE_KEY`) and replace the rest, so after an ownership transfer the new owner re-pins first ([ADMIN.md](deploy/ADMIN.md)).
