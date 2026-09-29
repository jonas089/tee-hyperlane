# DEPLOY

The whole bridge from nothing, on one Linux host, as `chef`. Every block starts with a `cd`,
so paste it from anywhere. Every script is safe to re-run.

A script that fails stops with `error`, naming the step or the command. One that could only do
part of its work (Eden before the mocha node has synced, say) exits 3 and lists what it left
undone: fix that and run it again.

Ids the scripts create go in `devnet/.state/out/`. List them with
`cd ~/tee-ism-nonzk && make status`.

On a laptop, `make init && make start` (in the repo root) runs the whole thing against a local
chain. Without Nix there, set `FORCE_UNPINNED=1` to skip step 5's pin checks. `make stop` deletes the enclaves and `.state/`, so **never run it on ark**.

## 0. What you need

- docker, go 1.26+, rust, foundry, node, python3, jq, curl, nix
- the phala CLI, logged in
- an EVM key used by nothing else, funded on Sepolia, Arbitrum Sepolia and Base Sepolia (ETH)
  and on Eden (TIA)
- a Phala balance (the enclaves cost $4.38/day)
- a free Alchemy key, for Base

Only port 3000 needs to be public.

## 1. Code and secrets

```sh
git clone https://github.com/jonas089/tee-hyperlane.git ~/tee-ism-nonzk
git clone -b jonas/tee-ism https://github.com/celestiaorg/celestia-app.git ~/celestia-app-local
cd ~/tee-ism-nonzk
ln -s ../../deploy/check-secrets.sh .git/hooks/pre-commit
mkdir -p devnet/.state && chmod 700 devnet/.state
cp devnet/.env.example devnet/.env && chmod 600 devnet/.env
$EDITOR devnet/.env      # set EVM_PRIVATE_KEY (with 0x) and ALCHEMY_BASE_KEY
```

celestia-app: always the `jonas/tee-ism` branch, never `main`.

## 2. Build

```sh
cd ~/celestia-app-local
for b in cmd/celestia-appd x/teeism/cmd/teeism-collateral x/teeism/cmd/teeism-identity; do
  go build -o ~/tee-ism-nonzk/devnet/.state/bin/$(basename $b) ./$b
done
cd ~/tee-ism-nonzk/tee-hyperlane && cargo build --release -p tee-coprocessor -p gas-oracle
```

## 3. Chain

```sh
cd ~/tee-ism-nonzk/devnet
./scripts/10-celestia-up.sh
./scripts/20-celestia-hyperlane.sh
grep -E '^pruning|^min-retain-blocks' .state/celestia/config/app.toml   # "nothing" and 0
```

## 4. Enclave images

```sh
cd ~/tee-ism-nonzk/devnet
docker login ghcr.io -u jonas089          # once; password: a GitHub token with write:packages
./scripts/25-images.sh
cd ~/tee-ism-nonzk && git add deploy/images.lock deploy/docker-compose.*.yml
git commit -m "Pin enclave images" && git push
```

`25-images.sh` builds, pushes and pins only the families whose code changed, and records each
in `deploy/images.lock`. Step 5 refuses to run until those pins match the code and are
pushed. Pushing from the server needs a GitHub token; otherwise copy the four files off and
commit them elsewhere.

## 5. Enclaves

```sh
cd ~/tee-ism-nonzk/devnet
./scripts/30-enclave-up.sh
for f in celestia ethereum evolve; do FAMILY=$f ~/tee-ism-nonzk/deploy/verify-digest.sh "$(cat .state/out/enclave-app-id-$f)"; done
```

It stops first if a family's code no longer matches its pin in `deploy/images.lock`, or the
pins aren't committed and pushed. Every `verify-digest.sh` check must say `ok`. Never use
`phala cvms upgrade`: it changes the enclave's identity.

## 6. Intel collateral on the EVM chains

```sh
cd ~/tee-ism-nonzk/devnet
scp -P 50 chef@<old host>:tee-ism-nonzk/devnet/.state/out/pccs-*.json .state/out/
for c in sepolia arbitrum base eden; do ./scripts/seed-evm-collateral.sh $c; done
```

These files hold the addresses of our Automata contracts. For a chain that has none yet, see
[appendix C](#c-automata-on-a-new-evm-chain).

## 7. Mocha light node (for Eden)

```sh
cd ~/tee-ism-nonzk/devnet
IMG=ghcr.io/celestiaorg/celestia-node:v0.34.2-mocha
D=$PWD/.state/mocha-light && mkdir -p "$D"
docker run --rm -v $D:/home/celestia -u "$(id -u):$(id -g)" $IMG celestia light init --p2p.network mocha
$EDITOR $D/config.toml      # [Share.LightAvailability] SampleAmount = 16; Header.Syncer.PruningWindow = "800h0m0s"
docker run -d --name mocha-light --restart unless-stopped -p 127.0.0.1:26658:26658 \
  -v $D:/home/celestia -u "$(id -u):$(id -g)" $IMG \
  celestia light start --p2p.network mocha --rpc.addr 0.0.0.0 --rpc.port 26658 --rpc.skip-auth
```

It takes about 30 minutes to sync. Until it has, step 8 skips Eden; re-run step 8 afterwards.

## 8. Tokens, ISMs, warp routes

```sh
cd ~/tee-ism-nonzk/devnet
./scripts/50-warp-celestia.sh
./scripts/75-dcap-verifiers.sh   # a v5 quote verifier on each EVM chain, next to v4
./scripts/80-evm-isms.sh
./scripts/85-celestia-isms.sh
./scripts/90-evm-warp.sh
. scripts/lib.sh; for k in ism-{sepolia,arbitrum,base,eden} ism-celestia-{sepolia,arbitrum,base,eden} routing-ism-id; do
  printf '%-24s %s\n' $k "$(load $k 2>/dev/null)"; done
```

Every line needs an id. **If `90-evm-warp.sh` prints `deploying the collateral USDC router` on
a chain that already has one, press Ctrl-C.** A new collateral router strands the USDC held in
the old one.

## 9. Paymaster and gas oracle

```sh
cd ~/tee-ism-nonzk/devnet && . scripts/lib.sh
A=.state/bin/celestia-appd; H="--home .state/celestia --keyring-backend test"
TX="$H --chain-id teeism-local --node http://localhost:26657 --fees 200000utia --gas 400000 -y"
$A tx hyperlane hooks igp create utia --from relayer $TX
IGP=<igp id from the output>
echo "$CELESTIA_MNEMONIC" | $A keys add bridge --recover --account 3 $H --output json
BRIDGE=$($A keys show bridge -a $H)
$A tx bank send relayer $BRIDGE 100000000utia $TX
$A tx hyperlane hooks igp set-owner $IGP --new-owner $BRIDGE --from relayer $TX
$A tx hyperlane mailbox set "$(load mailbox-id)" --required-hook "$(load merkle-hook-id)" --default-hook $IGP --from relayer $TX

cp ~/tee-ism-nonzk/deploy/gas-oracle.toml.example .state/gas-oracle.toml
sed -i "s|^igp_id = .*|igp_id = \"$IGP\"|" .state/gas-oracle.toml
printf '%s' "$EVM_PRIVATE_KEY" > .state/evm-key && chmod 600 .state/evm-key
~/tee-ism-nonzk/tee-hyperlane/target/release/gas-oracle --config .state/gas-oracle.toml --once
```

**EVM side, once per chain** (already done on Sepolia, Arbitrum and Base). Skipping it makes
transfers revert with `no gas oracle for domain`.

```sh
cast send <igp> "setDestinationGasConfigs((uint32,(address,uint96))[])" \
  "[(1297040299,(<oracle>,150000))]" --rpc-url <rpc> --private-key $EVM_PRIVATE_KEY
```

| chain | igp | oracle |
|---|---|---|
| Sepolia | `0x48b1BF6CC2e45Ca52947E95Bb216C2eBdCB19c49` | `0x225B8488242c90085B7A8Ea33Ce8e39Ae9f79722` |
| Arbitrum | `0x0ee6374a92ba4E11F920A23c6dd271b594D69A9B` | `0xfA8036Cb092079B095ed60750d7b39c3C220F288` |
| Base | `0x5591613C85E9bC95104980d4485c958ee80f6F76` | `0x7A7042C8784700618be87Aac7F9336620e216Bb9` |

**Router hooks.** Check each new EVM router's hook:

```sh
cast call <router> 'hook()(address)' --rpc-url <rpc>
```

If it prints `0x000…`, point it at the hook that chain's TIA router uses:

```sh
cast send <router> "setHook(address)" <that hook> --rpc-url <rpc> --private-key $EVM_PRIVATE_KEY
```

A router with no hook sends messages that can never be delivered.

## 10. Services

```sh
cd ~/tee-ism-nonzk/devnet && . scripts/lib.sh && write_config
sudo cp ~/tee-ism-nonzk/deploy/server/teeism-{relayer,gas-oracle}.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now teeism-relayer teeism-gas-oracle
curl -s localhost:3001/api/status | jq -r '.[] | "\(.name) \(.height)"'     # 8 routes
```

`write_config` writes `.state/coprocessor.toml`; don't edit that file by hand. The faucet is
part of the relayer and needs no setup.

## 11. UI and gateway

```sh
cd ~/tee-ism-nonzk/devnet && . scripts/lib.sh
HOST=http://<public host>:3000
opt() { has "$1" && load "$1" || true; }
cat > ~/tee-ism-nonzk/bridge-app/.env.local <<ENV
VITE_CELESTIA_NAME=Celestia teeism
VITE_CELESTIA_CHAIN_ID=teeism-local
VITE_CELESTIA_DOMAIN=1297040299
VITE_CELESTIA_RPC=$HOST/rpc
VITE_CELESTIA_REST=$HOST/rest
VITE_CELESTIA_EXPLORER=$HOST
VITE_RELAYER_API=$HOST/api
VITE_CELESTIA_MAILBOX_ID=$(load mailbox-id)
VITE_CELESTIA_ISM_ID=$(load routing-ism-id)
VITE_CELESTIA_IGP_ID=$IGP
VITE_CELESTIA_TIA_ROUTER=$(load celestia-token-id)
VITE_CELESTIA_USDC_ROUTER=$(opt celestia-usdc-token-id)
$(for c in sepolia arbitrum base eden; do C=$(echo $c | tr a-z A-Z)
  echo "VITE_${C}_RPC=$HOST/evm/$c/"
  echo "VITE_${C}_ISM=$(opt ism-$c)"
  echo "VITE_${C}_TIA_ROUTER=$(opt $c-router)"
  echo "VITE_${C}_USDC_ROUTER=$(opt $c-usdc-router)"; done)
VITE_PROVING_SECONDS=30
ENV
cd ~/tee-ism-nonzk/bridge-app && npm install --silent && VITE_DEVNET=1 npm run build
grep -c 'localhost:26657' dist/assets/index-*.js      # must print 0
cd ~/tee-ism-nonzk/devnet/gateway && UI_DIST=~/tee-ism-nonzk/bridge-app/dist docker compose up -d
```

Always build the UI on the server itself: the build bakes `.env.local` in.

## 12. Check

```sh
B=http://<public host>:3000
curl -s -o /dev/null -w '%{http_code}\n' $B/                  # 200
curl -s $B/rpc/status | jq -r .result.node_info.network       # teeism-local
curl -s $B/api/status | jq -r '.[] | "\(.name) \(.height)"'   # 8 routes
```

Then send 0.1 TIA each way ([INTERACT.md](INTERACT.md)).

---

## Appendix

### A. Adding an asset

No enclave or ISM changes.

1. Add a row to `TOKENS` in `scripts/90-evm-warp.sh`, and the Celestia side to
   `scripts/50-warp-celestia.sh`.
2. Re-run both scripts, set the new routers' hooks (step 9), `write_config`, and restart the
   relayer.
3. Add the routers to `.env.local` (step 11), rebuild the UI, and fund the collateral side.

### B. Adding a chain

**As an origin**, one file per crate, named after the chain, under the chain it relies on:

| file | implement | returns |
|---|---|---|
| `tee-node/src/ethereum/arbitrum.rs` | `origin::Origin` | `verify`: the verified state root. `merkle_tree`: the Hyperlane tree under it |
| `tee-coprocessor/src/origin/ethereum/arbitrum.rs` | `origin::Indexer` | `gather`: the inputs for those two. `index`: messages between heights. `bootstrap`: a genesis state |

Then register it:
- add the chain to `CHAINS` in its parent module
- add a `kind` arm in `tee-coprocessor/src/config.rs`
- add its table to `write_config`
- add a row to the ISM scripts

Rules:
- Contract addresses, storage slots and keys are constants in the chain's file, never inputs.
- The time is never an input.

**As a destination**, an EVM chain reuses `tee-coprocessor/src/destination/evm.rs` and
`TeeDcapIsm.sol` unchanged. Any other kind of chain needs an ISM that behaves like
`TeeDcapIsm`, and a `destination/<kind>.rs` implementing `destination::Destination`.

### C. Automata on a new EVM chain

In `devnet/automata/`:

1. `make deploy-helpers && make deploy-dao`
2. `forge script script/automata/DeployCrlV2.s.sol`
3. `deploy_versioned.sh` for `storage-v2`, `tcb-eval`, `versioned 20`, `fmspc-v2 20`
4. `grantRoles(deployer, 1)` on each versioned DAO and on the TcbEvalDao
5. copy the PCCS record to the attestation repo's
   `network-registry/deployment/current/<chain-id>/onchain_pccs.json`
6. point `dcap.json` at our entrypoint, then run `DeployRouter`, `deployEntrypoint()`,
   `DeployVerifier`
7. on the router: `setQeIdDaoVersionedAddr(20, …)`, `setFmspcTcbDaoVersionedAddr(20, …)`
8. `setCallerAuthorization(router, true)` on both storage contracts
9. write `.state/out/pccs-<chain>.json`, then run `scripts/seed-evm-collateral.sh <chain>`

Pitfalls:
- keep `Salt.sol` domain-separated
- `SKIP_ESTIMATE=true`, as a string
- send transactions one at a time
- TCB info and QE identity need hand-built calldata
- Eden accepts TCB info only in the V1 DAO
- `openzeppelin-contracts` must be v5.0.2

`75-dcap-verifiers.sh` adds the v5 verifier (`V5QuoteVerifier`, recorded in the same
file) once per chain. The deployed addresses differ per chain. `devnet/.state/out/pccs-<chain>.json` is the record
the scripts and ISMs read:

| contract | Sepolia | Arbitrum | Base | Eden |
|---|---|---|---|---|
| AttestationEntrypoint | `0x961D4408f512D4a169bD76433460d2981a70c71F` | same | same | `0x6d748C482E0Eae5b63b5664CDaBE2C4F0461B6B8` |
| V4QuoteVerifier | `0xFFd8Ddff9b7e9ce124A7fdddcd817bA4d8B37ab7` | same | same | `0x0D00346F2FE363BBf086f1F753f6A3426627Ea92` |
| PCCSRouter | `0xdA7336571D634bE002035Af6ec55F0816A2Ed263` | same | same | `0x2c86Cb1a65079D6389c7bb0cf375651a36e8E992` |
| PcsDao | `0x3c3fF9105e62228c7dA62C3bA04d24D320c4433C` | same | same | same |
| EnclaveIdentityDaoVersioned | `0x426B9aC0e424dEcC66e4C3a7d9293839e16D8fc1` | same | same | `0x4108d529200FE300EF38E7E68447E34bc8d4b891` |
| FmspcTcbDaoVersioned | `0x7BDA83918CAAD9b5EC7F88A24660167E90053690` | `0xe08E2eE491666702312Fc71566420e2C221DdeC3` | `0x06D080A8642803465500D6C9004Cc9CF48094EeD` | `0x61F9E7c62B7c3ade50aeD35065Ea7a8733417471` (V1) |
| TcbEvalDao | `0x03b1B658C34Bb7919A9cA2067d0055f7dD5C5495` | same | same | `0xDd0F2b5B38391f1Bdf47c670aA161d5Aed7310d7` |

### D. Endpoints

Defaults, each overridable in `devnet/.env`:

```
SEPOLIA_RPC       https://rpc.sepolia.ethpandaops.io
SEPOLIA_BEACON    https://ethereum-sepolia-beacon-api.publicnode.com
ARBITRUM_ARCHIVE  https://api.zan.top/arb-sepolia
ARBITRUM_LOGS     https://sepolia-rollup.arbitrum.io/rpc   (serves eth_getLogs over 1M blocks; publicnode caps at 50k)
ARBITRUM_RPC      https://sepolia-rollup.arbitrum.io/rpc
BASE_ARCHIVE      Alchemy, from ALCHEMY_BASE_KEY
BASE_RPC          https://sepolia.base.org
EDEN_ARCHIVE      https://ev-reth-eden-testnet.binarybuilders.services:8545/   (needs debug_executionWitness)
EDEN_RPC          https://rpc.testnet.eden.gateway.fm/
EDEN_DA_RPC       http://localhost:26658
```

Never use a metered key for logs: a large `eth_getLogs` sweep drains it and stalls every route.
