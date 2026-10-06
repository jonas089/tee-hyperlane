# MAINTAIN

Run on ark as `chef`. Every block starts with a `cd`, so paste it from anywhere.

## Monthly: Intel collateral

```sh
cd ~/tee-ism-nonzk/devnet
./scripts/collateral-status.sh                                              # what expires when
for c in sepolia arbitrum base eden; do ./scripts/seed-evm-collateral.sh $c; done
```

Skip it and the Celestia → EVM routes stop after 30 days with `TCBR`. Routes into Celestia
don't need it.

## Update: code only

For changes to the coprocessor, gas oracle, UI or scripts.

```sh
cd ~/tee-ism-nonzk && git pull
cd ~/tee-ism-nonzk/tee-hyperlane && cargo build --release -p tee-coprocessor -p gas-oracle
cd ~/tee-ism-nonzk/devnet && . scripts/lib.sh && write_config
sudo systemctl restart teeism-relayer teeism-gas-oracle
```

For UI changes, also rebuild the UI ([DEPLOY step 11](DEPLOY.md#11-ui-and-gateway)).

## Update: enclave code

For changes to `crates/tee-node`, `crates/hyperlane-types` or `Cargo.lock`. A changed enclave
gives its CVM a new identity, and every ISM that CVM attests needs replacing. With all five
chains in the shared CVM that is all 8 ISMs; a chain running in its own CVM (see
[One chain in its own CVM](#one-chain-in-its-own-cvm)) only moves its own. The scripts only
redo what changed.

In-flight transfers are kept; see [No message is lost](#no-message-is-lost).

**1. Pull, stop, back up**

```sh
cd ~/tee-ism-nonzk && git pull
sudo systemctl stop teeism-relayer
cp -a devnet/.state ~/teeism-state-$(date +%F)
cd ~/tee-ism-nonzk/tee-hyperlane && cargo build --release -p tee-coprocessor -p gas-oracle
```

Leave `devnet/.state/proofs/` in place. It holds the hints (`chains/<chain>/`) that let the
Sepolia and Eden routes rebuild their old ISM's light-client state and resume.

**2. Images:** [DEPLOY step 4](DEPLOY.md#4-enclave-images): build, pin, commit and push.

**3. Enclaves:** [DEPLOY step 5](DEPLOY.md#5-enclaves), with the same flags the chains were
deployed with: none for the shared CVM, `--<chain>` for a chain in its own. It refuses to run
until step 2's pins match the code and are pushed. The old CVMs keep running.

**4. ISMs and routers**

```sh
cd ~/tee-ism-nonzk/devnet
./scripts/75-dcap-verifiers.sh && ./scripts/80-evm-isms.sh && ./scripts/85-celestia-isms.sh && ./scripts/90-evm-warp.sh
```

`80-evm-isms.sh` also replaces an EVM ISM whose contract `VERSION` is older than the source's,
even when the enclave is unchanged. Each router should print `repointing`. **If one prints `deploying the synthetic ... router`,
press Ctrl-C.** A new router orphans the tokens the old one minted.

**5. Start**

```sh
cd ~/tee-ism-nonzk/devnet && . scripts/lib.sh && write_config
sudo cp ~/tee-ism-nonzk/deploy/server/teeism-relayer.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl start teeism-relayer
curl -s localhost:3001/api/status | jq -r '.[] | "\(.name) \(.height)"'     # 8 routes
```

**6. UI:** set the new `VITE_*_ISM` values and rebuild ([DEPLOY step 11](DEPLOY.md#11-ui-and-gateway)).

**7. Test:** one transfer each way per route.

**8. Clean up**, only once step 7 works:

```sh
phala cvms delete <old app id>        # each replaced CVM, ids in ~/teeism-state-*/out/enclave-app-id-*
```

Then update the ids in `README.md`.

**Rollback** (before step 8):

```sh
sudo systemctl stop teeism-relayer
cd ~/tee-ism-nonzk/devnet && rm -rf .state && cp -a ~/teeism-state-<date> .state
./scripts/85-celestia-isms.sh && ./scripts/90-evm-warp.sh      # routers back to the old ISMs
cd ~/tee-ism-nonzk && git checkout <previous commit>
cd ~/tee-ism-nonzk/tee-hyperlane && cargo build --release -p tee-coprocessor
sudo systemctl start teeism-relayer
```

## Replace an enclave, same image

```sh
phala cvms delete <old app id>
cd ~/tee-ism-nonzk/devnet
./scripts/30-enclave-up.sh                          # identity must match README.md
. scripts/lib.sh && write_config && sudo systemctl restart teeism-relayer
```

## One chain in its own CVM

By default every chain's enclave runs in the shared CVM. Moving one out gives it its own
identity, so later changes to it no longer replace the other chains' ISMs, and the reverse.
It costs one more CVM.

```sh
cd ~/tee-ism-nonzk/devnet
./scripts/30-enclave-up.sh --base                   # any of --celestia --ethereum --base --arbitrum --eden
./scripts/80-evm-isms.sh && ./scripts/85-celestia-isms.sh && ./scripts/90-evm-warp.sh
. scripts/lib.sh && write_config && sudo systemctl restart teeism-relayer
```

The scripts replace only the ISMs that trust the moved chain (`--celestia`: the four on the
EVM chains; any other: that origin's ISM on Celestia), resuming from their last state. The
shared CVM keeps serving the rest.

## No message is lost

When a script replaces an ISM, the new one starts from the old one's last state, with only the
enclave identity (its last 32 bytes) swapped. The route picks up exactly where it stopped, so
every message in flight is still delivered. If the old state can't be read, the script stops
instead of starting from the head.

A Celestia → EVM route can resume only if its last state is under 14 days old. The relayer's
12-hour heartbeat keeps it well within that.

## Waiting or stuck?

| route | normal wait |
|---|---|
| Celestia → any EVM | < 1 min |
| Eden → Celestia | 1-4 min, mostly waiting for Eden to post its headers to mocha |
| Sepolia → Celestia | ~15 min |
| Arbitrum → Celestia | < 1 min |
| Base → Celestia | < 1 min |

Base and Arbitrum are attested from the newest block their sequencer signed, so a transfer
from either that waits more than a few minutes is stuck: check the symptoms below.

`leaves=N` in the log counts other people's messages too.

## Symptoms

| you see | fix |
|---|---|
| `TCBR`, `PCKCRLH` or another collateral code | run the monthly job |
| `TCBR` right after the monthly job | Intel moved on; deploy a new versioned FMSPC DAO per chain |
| `WrongEnclave`, `IdentityChanged` | a route points at the wrong enclave; check the urls from `write_config` |
| `unknown command "teeism"` | the unit's PATH must start with `.state/bin` |
| `eth_getProof` fails at an old height | use an archive endpoint ([DEPLOY E](DEPLOY.md#e-endpoints)) |
| Sepolia: `eth_getProof` … `operation timed out` against `127.0.0.1:8545` | the trusted height drifted too far back for the full node, which rebuilds old state from the head. `max_lag` (from `write_config`) prevents it; to recover at once, point `SEPOLIA_RPC` at an archive for one pass |
| a Celestia-origin ISM stuck, trusted commit missing | the chain was pruned; replace the ISM |
| `no enrolled router found` | run `90-evm-warp.sh` |
| random `nonce too low` | another relayer uses the same EVM key |
| every route backing off | an endpoint is rate-limited |
| UI shows `Failed to fetch` | the UI was built with the wrong `.env.local`; rebuild on the host |
| `discarding a batch` | nothing; it rebuilds by itself |
| `carrying sync committee updates` every tick | nothing; the ISM is behind and catches up when a batch lands |
| Sepolia: `finality update rejected: invalid sync committee period` | a period boundary between the finalized header and its signature; the relayer carries the committee update for it since `7b51626`; before that it cleared within 20 minutes |
| `error decoding response body` once | a flaky RPC; nothing, unless it repeats |
| Eden: `no celestia header in the last 400 rebuilds this ISM's store` | the `da-heights` hint is gone; restore `.state/proofs/chains/eden/` from a backup |
| Eden: `no celestia block … carries an eden header at a height we hold a proof for` | normal for a few minutes after a restart; if it persists, check that mocha-light is synced |
| Base: `nothing from the base sequencer on p2p in Ns` | the p2p listener has no peers. `RUST_LOG=debug` shows `base gossip peers=… received=…`; check outbound TCP and UDP 9222 |
| Arbitrum: `nothing from the arbitrum sequencer feed in Ns` | the feed is unreachable; check `ARBITRUM_FEED` |
| `signed by 0x…, not the pinned sequencer 0x…` | the rollup rotated its sequencer key on L1. Set the new key in `tee-node/src/chains/l2/<chain>.rs` and roll out new enclave code |
| `proving the tree at signed block N` once | the free endpoint lags the sequencer by a block; nothing, unless it repeats |
| `reading the tree at the trusted height` | the Alchemy archive refused; check `ALCHEMY_KEY` and its quota |

To decode a four-letter code: `cast call <ism> "describeQuoteError(bytes)(string)" $(cast from-utf8 TCBR)`.

## Slack

Set `SLACK_BOT_TOKEN` and `SLACK_CHANNEL` (and `EXPLORER_URL` for links) in `devnet/.env`, then
restart the relayer. It posts on start, on every problem opening and resolving, and a status
every 30 minutes. **No status post for over 30 minutes means the relayer is down.**

## Check what runs

```sh
cd ~/tee-ism-nonzk
TARGET=all deploy/verify-digest.sh <app id>                                 # the shared CVM runs this repo
TARGET=all deploy/verify-digest.sh <app id> --ism <ism> --rpc <rpc>         # and the ISM accepts it
TARGET=base deploy/verify-digest.sh <app id>                                # a chain in its own CVM
cast call <router> 'interchainSecurityModule()(address)' --rpc-url <rpc>     # which ISM a router uses
```

`origin_domain` on the EVM ISMs reads `1297040200` (mocha's domain), not `1297040299`. That is
expected.

## Secrets

`devnet/.env`, `devnet/.state/` and `keys/` are gitignored. Run `deploy/check-secrets.sh`
before committing. If a key reaches GitHub, rotate it.
