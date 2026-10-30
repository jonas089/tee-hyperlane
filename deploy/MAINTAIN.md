# MAINTAIN

On ark as `chef`, in `~/tee-ism-nonzk`. After `git pull`, rebuild the CLI: `cargo install --locked --path cli`.

## Monthly: Intel collateral

```sh
teeism pccs status          # what expires when; exits 1 if anything is within 14 days
teeism pccs seed            # republish all of it; --dry-run checks and fetches without sending
```

- Skipped: Celestia -> EVM routes stop 30 days after the last seed, with `TCBR` or `PCKCRLH`. `pccs status` prints the date per chain; seed a week before it.
- Collateral is per CPU platform (FMSPC); only `20a06f000000`, the platform our CVMs ran on so far, is seeded. A CVM on a Phala node with a different CPU passes Celestia but fails every EVM route with `TCBR`: seed its platform, `teeism pccs seed --fmspc <hex>`.
- Celestia-destination routes need nothing: the relayer fetches fresh collateral with each attestation.
- Where it comes from and what it is checked against: [DEPLOY B](DEPLOY.md#b-intel-root-ca-and-collateral).

## Before each TCB recovery: the next evaluation number

Intel's standard evaluation number moves twelve months after release: **21 on about 10 Feb 2027, 22 on about 11 Aug 2027**. `teeism pccs deploy` covers every number Intel already publishes; for a newer one, weeks before:

```sh
teeism pccs tcb-eval 23      # its QE identity and TCB info contracts on every chain, registered, seeded
```

- Check the enclaves' host passes the new number, or ask Phala to patch it ([ADMIN](ADMIN.md#tcb-recovery)).
- Celestia needs nothing.

## Heartbeat

Every route attests at least daily, with no messages if nothing was dispatched, so its ISM stays near its origin's head and the next proof starts from recent state.

- The relayer sends one once an ISM's trusted state is a day old (`HEARTBEAT`, `tee-coprocessor/src/route.rs`).
- `TeeDcapIsm` and `x/teeism` refuse an attestation that leaves the tree root unchanged and moves the origin clock less than 30 minutes, so nobody can use the public enclave to advance an ISM more often with nothing in it.
- An attestation cannot skip a message: it carries the tree's root, and every message in the tree proves itself against that root at delivery (`root(32) index(4) proof(32 * 32)` in the Hyperlane metadata). Every root an ISM has attested stays accepted, so a message delivered late proves against the root it was attested under.
- Cost: one attestation (~3.87M gas on an EVM destination) per quiet route per day. On Sepolia at 1.1 gwei that is ~0.004 ETH a day.
- Check: `teeism status` lists every route's state age; over 26 hours is reported.

## Update: code only

Relayer, gas oracle, UI or CLI.

```sh
git pull && cargo install --locked --path cli
teeism build
teeism services restart
teeism ui --public-url http://178.199.12.26:3000      # UI changes only
```

## Rotate the enclave (re-pin)

For anything that changes what the ISMs pin: enclave code (`crates/tee-node`, `hyperlane-types`, `tee-hyperlane/Cargo.lock`, which pins helios), a new dstack OS image, another instance type, or Phala host maintenance that moved the measurements. Every ISM keeps its address and state; tokens, routers, the PCCS and the Celestia chain are untouched. Done this way on 2026-10-26 (dstack 0.5.12 -> 0.6.1).

```sh
git pull && git -C ../celestia-app-local pull && cargo install --locked --path cli && teeism build
cp -a devnet/.state ~/teeism-state-$(date +%F)            # backup
teeism images        # only if enclave code changed; enclaves up refuses otherwise. Then: git add deploy/ && git commit && git push
teeism celestia up --new-image                             # only if x/teeism changed (e.g. its identity)
teeism enclaves up && teeism enclaves verify               # new CVM; every verify line ok
teeism isms evm && teeism isms celestia                    # every line must say re-pinning
teeism services restart
teeism smoke                                               # Sepolia -> Celestia takes up to ~20 min
teeism enclaves delete --retired                           # only once smoke passed
```

- Anything saying `replacing`, or `belongs to an earlier deployment`, instead of `re-pinning`: stop. An ISM owned by a multisig is re-pinned from the multisig first ([ADMIN](ADMIN.md#re-pin-the-enclave-identity)).
- `isms celestia` failing with `invalid enclave identity`: the node still runs the old celestia-app; run `teeism celestia up --new-image` first.
- Expected while switching: routes failing for minutes (`enclave unreachable` on the old CVM, `localhost:26657 connection reset` during the node restart, one `account sequence mismatch`, `nothing from the base sequencer` after the restart). Nothing is lost; they recover on their own.
- Rollback before `delete --retired`: the old CVM still runs; re-pin to it ([ADMIN](ADMIN.md#re-pin-the-enclave-identity)).
- A changed `TeeDcapIsm` `VERSION` replaces each EVM ISM from its last state and re-points the routers (`teeism warp evm`). It never redeploys a router: a new router orphans what the old one minted.
- Then update the enclave section of `README.md`.

## Update: celestia-app

The chain runs the head of `jonas/tee-ism`. Its image is always `celestia-app-teeism:jonas-tee-ism`, rebuilt by every `celestia up` (seconds when nothing changed) and labelled with the commit it was built from; `teeism status` shows that commit next to the checkout's, and `celestia up` refuses to restart a live chain on a different build unless told to.

```sh
git -C ../celestia-app-local pull
teeism build                       # prints: celestia-app  jonas/tee-ism @ <commit> <date> <subject>
teeism celestia up --new-image     # rebuilds the image, restarts the chain on it
teeism services restart
teeism status                      # celestia: image commit == checkout commit
```

Only for changes that keep the state machine compatible with the existing chain. Anything else needs a coordinated upgrade or a fresh chain ([DEPLOY](DEPLOY.md)).

## One chain in its own CVM

```sh
teeism enclaves up --chain base     # any of celestia ethereum base arbitrum eden
teeism isms evm && teeism isms celestia && teeism warp evm
teeism services restart
```

Costs one more CVM.

## Tear everything down

```sh
teeism teardown            # lists what it would delete
teeism teardown --yes      # services, containers, CVMs, devnet/.state (devnet/.env is kept)
```

Contracts on the EVM chains stay where they are, unused. A CVM named `teeism-*` that the record does not know is reported, never deleted: `teeism enclaves delete --app-id <id>`.

## No message is lost

- A re-pin keeps each ISM's state; a replaced ISM resumes from the old one's last state, and stops rather than fall back to the head if it cannot read it.
- Every root an ISM attests stays accepted, so a message's proof never expires. A proof lands only against the root it was built for, though: if the ISM never attested that root (another relayer's attestation landed first and ours was discarded as stale; or the ISM was replaced and its root set started over), the message is parked with the tree it was proven from, and redelivery builds a new proof against the root the ISM holds now (`undelivered/<id>.json` carries `provenance`). Nothing to do by hand; the log says `re-proved against the root the ISM holds`.
- A message parked before the ISM covers it (its batch was discarded while the ISM sat between its start and end) is reported as `cannot re-prove yet` until the next attestation; then it lands, or the regular step delivers it first and the parked copy is found delivered.
- Celestia -> EVM resumes only if its last state is under 14 days old. Past that, a replacement ISM anchored at the head attests a root that contains every earlier leaf, so the messages are not lost, but the relayer only indexes forward from the ISM's height: recovering leaves dispatched during the outage needs a frontier at an earlier height, which is not tooled yet.

## Waiting or stuck?

| route | normal wait |
|---|---|
| Celestia -> any EVM | < 1 min |
| Eden -> Celestia | 1-4 min |
| Sepolia -> Celestia | ~15 min |
| Arbitrum -> Celestia | < 1 min |
| Base -> Celestia | < 1 min |

Base or Arbitrum waiting more than a few minutes: stuck, see below. `leaves=N` counts other people's messages too.

## Symptoms

| you see | fix |
|---|---|
| `TCBR`, `PCKCRLH` or another collateral code | `teeism pccs seed` |
| `TCBR` right after a seed | Intel moved the evaluation number: `teeism pccs tcb-eval <n>` |
| `WrongEnclave`, `IdentityChanged` | `teeism services restart` (rewrites the enclave URLs); `teeism enclaves verify --ism-chain <chain>` |
| `WrongEnclave` / `identity mismatch` on every route after Phala maintenance, with no change of ours | the host moved the measurements (e.g. a dstack 0.6 VMM changes RTMR0 at the CVM's next start): [rotate the enclave](#rotate-the-enclave-re-pin) |
| `TCBR` on every EVM route right after a new CVM | its Phala node has another CPU platform (FMSPC): `teeism pccs seed --fmspc <hex>` |
| `EmptyBatchTooSoon`, `an attestation with no new leaves must advance the origin clock` | nothing, unless it repeats: someone else advanced the ISM |
| `process` reverts or `verification failed` with the ISM's state ahead of the message | the delivery's metadata names a root the ISM never attested; redelivery re-proves it against the ISM's current root by itself (`re-proved against the root the ISM holds`). `cannot re-prove yet` names the cause: the ISM has not attested since (wait for the next attestation), or the record predates `provenance` (nothing can rebuild it: the regular step never indexes backwards). Never delete a parked message to "retry" it; the next pass starts from the ISM's height and would not see it again |
| `unknown command "teeism"` from the relayer | the unit's PATH must start with `.state/bin` |
| `eth_getProof` fails at an old height | use an archive endpoint ([DEPLOY G](DEPLOY.md#g-endpoints)) |
| Sepolia: `eth_getProof` ... `operation timed out` against `127.0.0.1:8545` | point `SEPOLIA_RPC` at an archive for one pass |
| a Celestia-origin ISM stuck, trusted commit missing | replace the ISM |
| `no enrolled router found` | `teeism warp evm` |
| random `nonce too low` | another relayer uses the same EVM key |
| every route backing off | an endpoint is rate-limited |
| UI shows `Failed to fetch` | `teeism ui` on the host |
| `discarding a batch` | nothing |
| `carrying sync committee updates` every tick | nothing |
| Sepolia: `GLOAS_FORK_VERSION missing` | point `SEPOLIA_BEACON` at Lodestar ([DEPLOY F](DEPLOY.md#f-sepolia-node)) |
| Sepolia: `no finalized checkpoint ... rebuilds this ISM's light-client store` | if `.state/proofs/chains/sepolia/store-*.json` is gone, re-anchor the Sepolia ISM |
| Sepolia deliveries: `ClockBehindAttest` | `SEPOLIA_SEND_RPC` must be at the head |
| `error decoding response body` once | nothing, unless it repeats |
| Eden: `no celestia header in the last 400 rebuilds this ISM's store` | restore `.state/proofs/chains/eden/` from a backup |
| Eden: `no celestia block ... carries an eden header at a height we hold a proof for` | wait a few minutes; if it persists, check mocha-light is synced |
| Base: `nothing from the base sequencer on p2p in Ns` | `RUST_LOG=debug` shows `base gossip peers=... received=...`; check outbound TCP and UDP 9222 |
| Arbitrum: `nothing from the arbitrum sequencer feed in Ns` | check `ARBITRUM_FEED` |
| `signed by 0x..., not the pinned sequencer 0x...` | set the new key in `tee-node/src/chains/l2/<chain>.rs` and roll out new enclave code |
| `proving the tree at signed block N` once | nothing, unless it repeats |
| `reading the tree at the trusted height` | check `ALCHEMY_KEY` and its quota |

Decode a four-letter verifier code: `cast call <ism> "describeQuoteError(bytes)(string)" $(cast from-utf8 TCBR) --rpc-url <rpc>`.

## Upstream changes

`teeism upstream check` runs daily (`teeism-upstream.timer`) and reports upgrades and releases that could break the bridge, ideally before they activate:

- **Schedules and keys, from the chains:** consensus forks in Sepolia's beacon spec, Base Sepolia's upgrade table in `base/base`, ArbOS versions and scheduled upgrades, and the Base and Arbitrum sequencer keys against the ones the enclave pins. A rule decides these.
- **Releases** of the clients, specs and contracts in `deploy/upstream.toml`, each judged by OpenAI against the manifest there.

`breaking` and `maybe` items show on the dashboard (Upstream tab) and in Slack.

```sh
teeism upstream list                     # what is open
teeism upstream ack <id> --note "..."    # handled: leaves the open list, not posted again
teeism upstream check                    # run now; --no-judge collects without calling OpenAI
teeism upstream eval                     # the judge on known history; after changing the manifest or OPENAI_MODEL
```

When the stack changes (a fork the enclave now handles, a new chain, a new pinned key), update `deploy/upstream.toml`: a supported fork goes in its list so it is no longer reported.

## Slack

Set `SLACK_BOT_TOKEN` and `SLACK_CHANNEL` (and `EXPLORER_URL` for links) in `devnet/.env`, then `teeism services restart`. **No status post for over 30 minutes means the relayer is down.**

## Check what runs

```sh
teeism status                                     # everything deployed, and every route's state age
teeism enclaves verify                            # the shared CVM runs this repo's compose
teeism enclaves verify --ism-chain sepolia        # and that chain's ISM accepts it
teeism enclaves verify --target base              # a chain in its own CVM
teeism pccs check-quote --enclave celestia        # each EVM verifier accepts a fresh quote
```

`origin_domain` on the EVM ISMs reads `1297040200`, not `1297040299`. Expected.

## Secrets

`teeism check-secrets` runs before every commit (`teeism check-secrets --install-hook`). If a key reaches GitHub, rotate it.

## Intel's side breaks

A re-pin doesn't fix it. See [ADMIN.md](ADMIN.md#when-something-breaks-live).
