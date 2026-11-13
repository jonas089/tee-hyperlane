# DEPLOY

From nothing to eight live routes, one `teeism` command per step. On ark as `chef`, in `~/tee-ism-nonzk`.

- Every command is safe to re-run: it reads what exists first and records what it creates in `devnet/.state/deployment.json`.
- Any failure stops with what it was doing and why. Exit 3: partly done (e.g. Eden before mocha syncs); fix the cause, run it again.
- Each step names the source file that implements it (map: `cli/src/commands.rs`). Read it before running the step.

## 0. What you need

- docker, go 1.26+, rust, foundry, node, git, nix (x86_64 Linux, sandbox on), the `phala` CLI.
- A Phala Cloud API key with balance ($4.38/day per CVM), a free Alchemy key.
- The EVM key (`EVM_PRIVATE_KEY`): owns every contract, relays, seeds collateral. Fund it on Sepolia, Arbitrum Sepolia, Base Sepolia (ETH) and Eden (TIA). The PCCS deploy is the expensive part: budget 0.2 ETH on Sepolia.
- Sepolia node on the host: reth (`127.0.0.1:8545`) and Lodestar (`127.0.0.1:9596`, serves Gloas light-client data). See [F](#f-sepolia-node).
- Inbound: port 3000 only. Outbound: Base p2p (TCP and UDP 9222).

## 1. Code, secrets, the CLI

```sh
git clone https://github.com/jonas089/tee-hyperlane.git ~/tee-ism-nonzk
git clone -b jonas/tee-ism https://github.com/celestiaorg/celestia-app.git ~/celestia-app-local
cd ~/tee-ism-nonzk
cp devnet/.env.example devnet/.env && chmod 600 devnet/.env
$EDITOR devnet/.env            # EVM_PRIVATE_KEY (with 0x), ALCHEMY_KEY; SLACK_* optional
cargo install --locked --path cli
teeism check-secrets --install-hook
phala auth login <api key>     # cloud.phala.network > Settings > API Keys
docker login ghcr.io -u jonas089   # a GitHub token with write:packages, for step 6
```

celestia-app: only ever `jonas/tee-ism`, at its head; `teeism build` refuses any other branch and prints the commit it has.

## 2. Preflight

`cli/src/steps/preflight.rs`. Read only.

```sh
teeism preflight
```

Checks tools, the celestia-app branch, keys, Phala login, each EVM endpoint and the signer's balance there, the Sepolia node, Intel's PCS. Every line must say `ok`.

## 3. Build

`cli/src/steps/build.rs`.

```sh
teeism build
```

- `celestia-appd`, `teeism-collateral`, `teeism-identity` from celestia-app into `devnet/.state/bin/`.
- The relayer (`tee-hyperlane`) and `gas-oracle`.
- `tee-hyperlane/contracts`: TeeDcapIsm, and Hyperlane's Mailbox, MerkleTreeHook, HypERC20.
- Automata's PCCS and DCAP verifier, after fetching their Solidity libraries at the revisions their `foundry.lock` pins.

## 4. The Celestia chain

`cli/src/steps/celestia/up.rs`, `hyperlane.rs`.

```sh
teeism celestia up      # genesis (first run only), then the node in docker
teeism celestia core    # mailbox, merkle tree hook, noop ISM, noop hook
teeism celestia warp    # TIA collateral token; teeUSD synthetic, 1B minted to `user`, supply then fixed
grep -E '^pruning|^min-retain-blocks' devnet/.state/celestia/config/app.toml   # "nothing" and 0
```

- Keys derive from `CELESTIA_MNEMONIC` (generated into `devnet/.env` if empty): `user` 0 (Keplr's account; the faucet pays from it), `relayer` 1, `validator` 2, `bridge` 3 (step 11), `smoke` 5.
- The node image is `celestia-app-teeism:jonas-tee-ism`, rebuilt by every `celestia up` from the checkout and labelled with its commit. On a running chain, `celestia up` refuses a different build unless `--new-image`.

## 5. Mocha light node (for Eden)

`cli/src/steps/celestia/mocha.rs`. Start it now: it syncs for about 30 minutes, and step 9 needs it for Eden.

```sh
teeism mocha up
```

## 6. Enclave images

`cli/src/steps/enclaves/images.rs`, `flake.nix`.

```sh
teeism images
```

- Builds each chain's image with Nix, pushes it, pins the digest in `deploy/docker-compose.<chain>.yml` and `deploy/images.lock`, regenerates `docker-compose.all.yml`. A chain whose source is unchanged is skipped.
- The pins must be committed and pushed before step 7. Add only these files: `git add .` would also take untracked files such as an `.env` backup.

```sh
git add deploy/ && git commit -m "Pin enclave images" && git push
```

## 7. Enclaves

`cli/src/steps/enclaves/up.rs`, `verify.rs`.

```sh
teeism enclaves up
teeism enclaves verify
```

- Refuses unless every image is what this source builds and the pins are pushed (`--unpinned` skips that; never on ark).
- One CVM on Phala (`--no-dev-os`); Phala picks the node, and the CLI records its gateway, from which every enclave URL is built running all five enclaves, ports 8080 to 8084. `--chain <name>` runs one alone instead.
- Then reads each enclave's quote: the identity the Celestia ISMs pin, the measurements the EVM ISMs pin, mr_td.
- `--instance-type` is the VM shape, default `tdx.small`. `--os-image` is the dstack OS image, default `dstack-0.6.1`. It must be a 0.6 image: the enclave identity (`x/teeism` identity v3) pins mr_td, RTMR0-2 and `os-image-hash`, `compose-hash` and `key-provider`, and has no `mr-kms`, which only 0.5 recorded. The node, URL and gateway are plumbing: the ISMs trust only the quote's measurements.
- `verify` checks compose file -> `compose_hash` -> `mr_config_id` in the signed quote (dstack 0.6 binds the compose hash through a document, `mr_config_id` v3, which `verify` rebuilds and matches). Every line must say `ok`. `--rebuild` also rebuilds the images and compares them.
- **Never `phala cvms upgrade`: it changes the enclave identity.**

## 8. Intel's collateral on the EVM chains (PCCS)

`cli/src/steps/pccs/deploy.rs`, `seed.rs`, `status.rs`. Background: [B](#b-intel-root-ca-and-collateral).

```sh
teeism pccs deploy
teeism pccs check-quote --enclave celestia
teeism pccs status
```

- `deploy`: per chain, Automata's on-chain PCCS and DCAP verifier through the CREATE2 deployer (a contract the chain's record already names, with code at that address, is kept rather than recomputed from bytecode), salts `keccak256("tee-isms/v2:<NAME>")`: helpers, storage, CRL v2 PCS and PCK DAOs, TCB evaluation DAO, router, entrypoint, v4 and v5 quote verifiers, and per TCB evaluation number (Intel's standard and every newer one it publishes) a QE identity and a TCB info DAO. Then seeds them from Intel.
- `check-quote`: the Celestia enclave's quote through each chain's verifier, exactly as its ISM will.
- `status`: every artifact `ok` and valid for more than 14 days.
- `--namespace` picks another salt prefix: a new, independent deployment.

## 9. ISMs

`cli/src/steps/mailbox.rs`, `cli/src/steps/isms.rs`.

```sh
teeism mailbox            # Mailbox and MerkleTreeHook on EVM chains without a canonical Hyperlane (Eden)
teeism isms evm           # a TeeDcapIsm per EVM chain, pinning the Celestia enclave; initializes Eden's mailbox
teeism isms celestia      # an x/teeism ISM per EVM origin, the routing ISM, tokens and mailbox pointed at it
```

- New ISMs anchor at each origin's current head.
- `isms celestia` exits 3 for Eden until mocha has synced (`docker logs mocha-light`); run it again.

## 10. Warp routes

`cli/src/steps/warp.rs`.

```sh
teeism warp evm
```

A TIA and a teeUSD router (HypERC20) per EVM chain, ISM set at initialize, enrolled with the Celestia token, and the Celestia tokens enrolled with them.

## 11. Paymaster

`cli/src/steps/celestia/paymaster.rs`.

```sh
teeism paymaster
```

The interchain gas paymaster on Celestia, owned by the `bridge` key, as the mailbox's default hook (required hook: the merkle tree hook). Posts each destination's gas price once.

## 12. Services

`cli/src/steps/services.rs`, `deploy/server/*.service`.

```sh
teeism services install      # sudo: units into /etc/systemd/system, enable, start
```

Writes `devnet/.state/coprocessor.toml` and `gas-oracle.toml` from the deployment record, starts `teeism-relayer` and `teeism-gas-oracle`, waits for all 8 routes. Never hand-edit those two files: `teeism config` rewrites them.

## 13. UI and gateway

`cli/src/steps/ui.rs`, `devnet/gateway/`.

```sh
teeism ui --public-url http://178.199.12.26:3000
```

Writes `bridge-app/.env.local` from the deployment, builds the UI, serves it with `/rpc /rest /api /evm/<chain>/` through nginx on :3000.

## 14. End to end

`cli/src/steps/smoke.rs`, `cli/src/steps/status.rs`.

```sh
teeism smoke
teeism status
```

- `smoke`: 0.1 TIA and 0.1 teeUSD Celestia -> each EVM chain, then back from each to the `smoke` key: 16 transfers on 8 routes, each waited on until it arrives.
- `status`: every id, every enclave, every route's ISM height and the age of its trusted state. Exits 1 if a route is not advancing.
- Run `status` again over the next day: every route's state stays under a day old plus its origin's lag, from the daily heartbeat ([MAINTAIN](MAINTAIN.md#heartbeat)).

Then copy `teeism status` into `README.md` and refresh `deploy/coprocessor.toml.example`.

---

## A. Re-pin with the admin key: a new enclave measurement

The ISMs pin `keccak256(mr_td ++ mr_config_id ++ rtmr0..2)` (EVM) and the identity (Celestia), which cover the dstack OS image, the VM's vCPU and memory shape and the compose. Changing any of them means every ISM is re-pinned by its owner, in place: same address, same state, no router moves. First done on 2026-10-26 (dstack 0.5.12 -> 0.6.1); the full routine, with the image and celestia-app steps, is [MAINTAIN: rotate the enclave](MAINTAIN.md#rotate-the-enclave-re-pin).

```sh
teeism enclaves up                            # a new CVM (dstack-0.6.1, tdx.small); the old one is recorded as retired
teeism enclaves verify
teeism isms evm                               # setEnclaves on each TeeDcapIsm, signed by EVM_SIGNER
teeism isms celestia                          # tx teeism set-identities on each, signed by `relayer`
teeism services restart                       # the relayer moves to the new enclave URLs
teeism smoke
teeism enclaves delete --retired              # only once smoke passed
```

- `enclaves up` prints the new measurements; `isms evm` and `isms celestia` print `re-pinning <ism>: <old> -> <new>` per ISM. Anything saying `replacing` instead means an ISM this key does not own or an older contract revision: stop and check.
- Until `services restart`, attestations from the old enclave are refused (`WrongEnclave`); nothing is lost, the relayer retries. `enclaves up --add` avoids even that: the ISMs pin old and new together while the relayer moves ([MAINTAIN](MAINTAIN.md#rotate-the-enclave-re-pin)).
- Rollback before `delete --retired`: the old CVM still runs; re-pin to it with [ADMIN](ADMIN.md)'s commands.

## B. Intel root CA and collateral

What every quote is checked against, and where each piece comes from.

**The root of trust is Intel SGX Root CA**, self-signed, valid to 2049:

```sh
curl -sO https://certificates.trustedservices.intel.com/Intel_SGX_Provisioning_Certification_RootCA.cer
openssl x509 -inform der -in Intel_SGX_Provisioning_Certification_RootCA.cer -noout -subject -fingerprint -sha256
# CN=Intel SGX Root CA
# SHA256 Fingerprint=44:A0:19:6B:2B:99:F8:89:B8:E1:49:E9:5B:80:7A:35:0E:74:24:96:43:99:E8:85:A7:CB:B8:CC:FA:B6:74:D3
```

- **Celestia (`x/teeism`)** has it built in: go-tdx-guest's `verify/trusted_root.pem`, same fingerprint. Only the ISM owner can add another (`set-verifier-overrides --trusted-root-cert`, [ADMIN](ADMIN.md#update-the-intel-root-ca)).
- **EVM (PCCS)** hard-codes the hash of its public key, `ROOT_CA_PUBKEY_HASH` in `src/bases/PcsDaoV2.sol` of Automata's PCCS (`teeism build` checks it out under `tee-hyperlane/contracts/vendor/automata/pccs`), and refuses any other root. Check it:

```sh
openssl x509 -inform der -in Intel_SGX_Provisioning_Certification_RootCA.cer -pubkey -noout \
  | openssl ec -pubin -outform der 2>/dev/null | tail -c 64 | xxd -p -c 64 | xargs -I{} cast keccak 0x{}
# 0x89f72d7c488e5b53a77c23ebcb36970ef7eb5bcf6658e9b8292cfbe4703a8473
```

**Collateral**, all from Intel's PCS (`api.trustedservices.intel.com`), all signed by a chain ending in that root, so whoever uploads it needs no trust:

| artifact | from | valid |
|---|---|---|
| root CA, TCB signing certificate | `TCB-Info-Issuer-Chain` header of `tdx/certification/v4/tcb` | years |
| PCK platform and processor CA certificates and CRLs | `sgx/certification/v4/pckcrl?ca=platform` / `processor`, and its issuer chain header | 30 days |
| root CA CRL | the root certificate's CRL distribution point | months |
| TCB evaluation numbers, SGX and TDX | `.../tcbevaluationdatanumbers` | 30 days |
| TDX TCB info per evaluation number, FMSPC `20a06f000000` | `tdx/certification/v4/tcb?fmspc=&tcbEvaluationDataNumber=` | 30 days |
| TD_QE identity per evaluation number | `tdx/certification/v4/qe/identity?tcbEvaluationDataNumber=` | 30 days |

- EVM: `teeism pccs seed` uploads all of it; the DAOs verify each signature. Monthly ([MAINTAIN](MAINTAIN.md#monthly-intel-collateral)).
- Celestia: the relayer fetches the same collateral fresh with every attestation (`teeism-collateral`). Nothing to do. `x/teeism` refuses collateral older than 14 days, or older than the newest an ISM has accepted, so a revoked platform cannot attest with the collateral from before.

## C. Signing on a Ledger

```sh
cargo install --locked --path cli --features ledger
echo 'EVM_SIGNER=ledger' >> devnet/.env       # EVM_LEDGER_PATH for a non-first account
```

Ethereum app open, blind signing on. The Ledger then owns every contract `teeism` deploys and signs every owner transaction; `EVM_PRIVATE_KEY` only relays and seeds collateral (`pccs deploy` grants it the uploader role).

## D. Adding a chain

**Origin**: one file per crate, under `l1/` (light client) or `l2/` (rollup).

| file | implement | returns |
|---|---|---|
| `tee-node/src/chains/l2/arbitrum.rs` | `origin::Origin` | `verify`: the verified state root. `merkle_tree`: the Hyperlane tree under it |
| `tee-coprocessor/src/origin/l2/arbitrum.rs` | `origin::Indexer` | `gather`: the inputs for those two. `index`: messages between heights. `bootstrap`: a genesis state |

secp256k1 sequencer: reuse `tee-node/src/chains/l2/sequencer.rs` and `tee-coprocessor/src/origin/l2/sequenced.rs`.

Register it:
1. cargo feature in `tee-node/Cargo.toml`, named after the chain; chain in `origin::chains()`
2. image: chain in `flake.nix` (`chains`, `chainFiles`, `keeps`), a `deploy/docker-compose.<chain>.yml`, its port in `ENCLAVES` (`cli/src/shared/chains.rs`)
3. the same feature in `tee-coprocessor/Cargo.toml`, forwarding to tee-node's and enabling the chain's own dependencies; both `default` lists; the module behind `#[cfg(feature = ...)]` in `origin/l1` or `origin/l2`; `kind` arms in `config.rs` (`indexer`, `destination`) and `tracker.rs` (`Watcher::new`); the kind in `config::KINDS`
4. its entry in `EVM_CHAINS` (`cli/src/shared/chains.rs`) and its table in `cli/src/steps/services.rs` (`coprocessor_toml`)

Rules:
- Addresses, storage slots, keys: constants, never inputs. Time: never an input.
- A new image changes the shared CVM's identity and every ISM attested from it.

**Destination**: EVM reuses `tee-coprocessor/src/destination/evm.rs` and `TeeDcapIsm.sol`. Others need a `TeeDcapIsm`-like ISM and `destination/<kind>.rs`.

## E. Adding an asset

No enclave or ISM changes. Add it to `ASSETS` in `cli/src/steps/warp.rs` and create its Celestia token in `cli/src/steps/celestia/hyperlane.rs` (like teeUSD), then `teeism celestia warp`, `teeism warp evm`, `teeism services restart`, `teeism ui`.

## F. Sepolia node

| | client | default (override in `devnet/.env`) |
|---|---|---|
| execution | reth 2.7.0+ | `127.0.0.1:8545` (`SEPOLIA_RPC`) |
| beacon | Lodestar v1.49.0+ | `127.0.0.1:9596` (`SEPOLIA_BEACON`) |

- reth: ~1 day of history and `--rpc.eth-proof-window` (ark: 120k blocks).
- Not lighthouse (404s after Glamsterdam). Lodestar uses the same JWT secret.

```sh
curl -s localhost:9596/eth/v1/beacon/light_client/finality_update | jq -r .version   # gloas
```

## G. Endpoints

Defaults, each overridable in `devnet/.env`:

```
SEPOLIA_RPC       http://127.0.0.1:8545   (reth; the relayer's reads)
SEPOLIA_SEND_RPC  SEPOLIA_RPC             (transactions; must be at the head)
SEPOLIA_BEACON    http://127.0.0.1:9596   (Lodestar)
ARBITRUM_ARCHIVE  Alchemy, from ALCHEMY_KEY
ARBITRUM_FEED     wss://sepolia-rollup.arbitrum.io/feed
ARBITRUM_LOGS     https://sepolia-rollup.arbitrum.io/rpc   (eth_getLogs over 1M blocks; publicnode caps at 50k)
ARBITRUM_RPC      https://sepolia-rollup.arbitrum.io/rpc
BASE_ARCHIVE      Alchemy, from ALCHEMY_KEY
BASE_RPC          https://sepolia.base.org
BASE_P2P_PORT     9222   (UDP, for discovery; outbound only)
EDEN_ARCHIVE      https://ev-reth-eden-testnet.binarybuilders.services:8545/
EDEN_RPC          https://rpc.testnet.eden.gateway.fm/
EDEN_DA_RPC       http://localhost:26658   (mocha light node)
CELESTIA_RPC      http://localhost:26657
```

Never a metered key for logs: `eth_getLogs` sweeps drain it and stall every route.
