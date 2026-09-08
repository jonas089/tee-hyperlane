# Server deployment

One machine runs the coprocessor, the attestation API, the gas oracle and the UI. The two
enclaves stay on Phala; nothing here holds a TEE, and nothing here is trusted by any ISM.

## Install

`install.sh` does the whole thing and is safe to re-run - that is how upgrades are applied.

```sh
git clone <repo> /opt/tee-isms
cd /opt/tee-isms/tee-hyperlane && cargo build --release
cd /opt/tee-isms/bridge-app && npm ci && npm run build
bash /opt/tee-isms/deploy/server/install.sh
```

It creates the `bridge` service user, lays out `/opt/tee-hyperlane`, imports the relayer's
Celestia key, installs three units and the nginx site, and starts everything.

Build prerequisites, all of which `install.sh` assumes are present: rust, node, nginx,
`celestia-appd` on the PATH, plus **Go and libclang** - SP1's gnark FFI needs both, and its
build failure names neither.

## What ends up where

| | |
|---|---|
| `:3000` | bridge UI (nginx), with `/api` proxied to the relayer and `/celestia` to Mocha's REST |
| `:3001` | relayer dashboard and JSON API |
| `:3002` | gas oracle dashboard and JSON API |
| `/opt/tee-hyperlane` | binaries, submit scripts, keys, `coprocessor.toml`, `gas-oracle.toml` |
| `/var/lib/tee-hyperlane` | proof store, Celestia keyring, SP1 artifact cache |

Mocha's public REST answers correctly but sends no `Access-Control-Allow-Origin`, so the
browser cannot read it directly. That is why `/celestia` is proxied rather than called from
the page.

## The gas oracle

`gas-oracle.service` reads gas prices and token prices once an hour and writes them to both
sides: Celestia's IGP for outbound transfers, and each EVM chain's `StorageGasOracle` for
inbound ones. Without it, quotes are whatever was last written - or zero, on a fresh oracle.

```sh
gas-oracle --config gas-oracle.toml --once   # one round, printed, then exit
```

Use `--once` to check a config change before restarting the service. Two things it gets right
that are easy to get wrong by orders of magnitude: the 10^12 decimal gap between `utia` and
`wei`, and Celestia's sub-integer gas price, which is carried scaled by 10^6 with a matching
correction. Both are covered by tests.

Fees are charged to the sender and accrue to the beneficiary of whichever paymaster was used.
On the EVM side that is our own IGP, so they are claimable by the address configured at
deploy time; on Celestia they accrue in `utia` to the IGP owner.

## Sizing

Proving is two Groth16 wraps per batch. On a modern core each takes about 280 s and peaks
near 16 GB. A smaller box works but wants tuning - `SHARD_SIZE` is the memory lever, and
`tee-hyperlane.service` sets `2^20` because the default `2^22` puts a 8 GB machine into swap,
which costs far more time than the extra shards do. Give it 8+ real cores and 32 GB if you
want batches not to queue.

## Keys

`/opt/tee-hyperlane/keys` holds the relayer's EVM key, and the Celestia key is imported into
the keyring under `/var/lib/tee-hyperlane/celhome`. Both are only relayer keys: they pay gas
and can stall the bridge, but neither can make any chain accept a message the enclave did not
attest. `cast` and `celestia-appd` must be on the service PATH - the relayer shells out to
them to sign rather than reimplementing two transaction formats.

Replacing an enclave, and why nothing goes stale when you do, is in
[docs/redeploy.md](../../docs/redeploy.md).
