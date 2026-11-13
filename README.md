# TEE ISMs

Hyperlane bridging between Celestia and EVM chains. Messages are authorised by a **TDX enclave that verifies the origin chain**, checked on the destination via its TDX quote. No zero-knowledge proofs.

| doc | for |
|---|---|
| [deploy/DEPLOY.md](deploy/DEPLOY.md) | standing up the bridge |
| [deploy/MAINTAIN.md](deploy/MAINTAIN.md) | monthly job, rollouts, diagnosing a route |
| [deploy/ADMIN.md](deploy/ADMIN.md) | owner controls and commands, Intel breakage |
| [deploy/INTERACT.md](deploy/INTERACT.md) | wallets, sending, arrival, latency, cost |

## Layout

```
tee-hyperlane/crates/tee-node/          the enclave
tee-hyperlane/crates/tee-coprocessor/   the service: every route, the API, the faucet
tee-hyperlane/contracts/                TeeDcapIsm.sol; Automata's PCCS and DCAP verifier are fetched at pinned commits by `teeism build`
cli/                                    teeism: deploys, rotates, inspects and tears down everything
bridge-app/                             React UI, MetaMask + Keplr
devnet/                                 the chain's and the gateway's compose files
deploy/                                 the guides, the measured compose files, systemd units
```

- `l1/`: light-client chains (Celestia, Ethereum). `l2/`: sequencer-signed rollups (Base, Arbitrum, Eden).
- Per chain: `tee-node/src/chains/l2/base.rs` (verification), `tee-coprocessor/src/origin/l2/base.rs` (fetching).
- Features mean one thing: which chains are compiled in. `celestia`, `ethereum`, `base`, `arbitrum`, `eden`, named alike in `tee-node` and `tee-coprocessor`, each coprocessor feature forwarding to the enclave's. `default` builds every chain, for development, tests and the devnet. An enclave image is built with one (`flake.nix`); a relayer with the chains it serves. The CLI's `ledger` is the one feature that is not a chain. Check every combination: `cargo hack check -p tee-coprocessor --each-feature --exclude-no-default-features`.

## Deployments

First deployed with `teeism` on 2026-10-15; redeployed from a fresh devnet on 2026-10-26 (dstack 0.6.1 enclave, identity v3 pinning RTMR0-2). On 2026-10-30 the enclave moved to root attestation: new CVM, the four EVM ISMs replaced (`TeeDcapIsm` `VERSION` 5) from their predecessors' last state and the routers re-pointed, the four Celestia ISMs re-pinned in place, the chain restarted on celestia-app `1c3d993`. On 2026-11-09 every ISM moved to pinning a set of enclaves: the chain restarted in place on celestia-app `8667a693` (since `aaed6bc`: attestations priced and capped, the event log bounded, stale collateral refused) and each Celestia ISM was given its set with `tx teeism set-identities`; the four EVM ISMs were replaced (`VERSION` restarts at 1, pre-release) from their predecessors' last state and the routers re-pointed; a new CVM on another Phala node moved the compose hash, so all eight ISMs were re-pinned in place; Eden's routers were put behind a routing ISM and a routing hook of ours, as mainnet will be. On 2026-11-10 the images were rebuilt on a refreshed lock (rustls 0.23.46 and rustls-webpki 0.103.16, the TLS floor tendermint-rs 0.41 sets; the light client itself stays at 0.40.4 until `celestia-types` moves) and rolled out with the first zero-downtime rotation: a second CVM next to the first, every ISM re-pinned in place to the set of both, the relayer moved, the old CVM retired and every ISM re-pinned to the new one alone. No contract or ISM was redeployed. Every id below is from those days. `teeism status` prints the live ids. Verify: `teeism enclaves verify`.

**Enclaves.** Phala Cloud (node picked by Phala), `tdx.small`, dstack `0.6.1`, image `ghcr.io/jonas089/tee-node`. All five in one CVM (`deploy/docker-compose.all.yml`); `teeism enclaves up --chain <chain>` runs one alone.

| enclave | port | attests |
|---|---|---|
| `celestia` | 8080 | Celestia |
| `ethereum` | 8081 | Sepolia |
| `base` | 8082 | Base Sepolia |
| `arbitrum` | 8083 | Arbitrum Sepolia |
| `eden` | 8084 | Eden |

```
app id         e56005780efc9e267b602647684a15196ffa439b
urls           https://e56005780efc9e267b602647684a15196ffa439b-<port>.dstack-pha1-prod9.phala.network
mr_td          0x2a674327c50218dba880066b349b8d559d749ed68dce33fd651c184a877d084b07a9e583767a7ad5da13ed91deec2b70
compose_hash   0xf440a71354bfba458534cf104fe048afa8eb67fe7481d7dc35611aad82eef373
os_image_hash  0xec43cb8c5e04b6bb6c53b33b16076a01d83d01f7f4b703f2f78a0d947d6247e6
rtmr0          0x68102e7b524af310f7b7d426ce75481e36c40f5d513a9009c046e9d37e31551f0134d954b496a3357fd61d03f07ffe96
rtmr1          0x89ef86c5125b986c325f0a94acfa87718a4ef3a4d4f237dbb72dda3cf777fd2c054555fb4407eb53338d7d67329a7333
rtmr2          0xe9000e338f33233e8f51b7fd031e1c158bdcf8572eab041c0bc8eb7fef48b23259a6a556e6c8e2af99999fc39c793179
identity       0x441a9c5698a48dca085b80815549023bb0867ade1c958b1a8952c3ff72fc9f50
measurements   0x9a2bb5d75308c1da58351dffd9f3d97e9d4e694d6c7d8109ed4f2a7faa081726
```

The previous CVMs (`3c591bb6…` on `dstack-pha-prod5`, deleted 2026-11-09; `109054f6…` on `dstack-pha1-prod9`, retired 2026-11-10) had the same mr_td, RTMR0-2 and OS image; only `compose_hash` moved, because dstack's app compose carries the CVM's name and the image digests. A new CVM is therefore always a re-pin, even on an unchanged image.

Image digests per chain: `deploy/images.lock`.

EVM ISMs pin `measurements`; Celestia ISMs pin the fields behind `identity`.

**Chain.** Own devnet (not mocha), pruning off.

```
chain id   teeism-local
domain     1297040299
mailbox    0x68797065726c616e650000000000000000000000000000000000000000000000
merkle     0x726f757465725f706f73745f6469737061746368000000030000000000000000
igp        0x726f757465725f706f73745f6469737061746368000000040000000000000002
```

**ISMs.**

| Celestia-origin, `TeeDcapIsm` on | address |
|---|---|
| Ethereum Sepolia | `0x015ee7e36961E840fC3119B94657703F3cFaD420` |
| Arbitrum Sepolia | `0xe1e0893ED28AAe38997c5d5eeb9AB7BF3Ce05a4E` |
| Base Sepolia | `0xA50F8683a5eb315BCc45457C9Ff65c273D60Fa88` |
| Eden | `0xBaB7AA2F7cCaBD98cEb82EddD1DBe93D60356892` |

Revision 1 (a pinned set of enclaves; `VERSION` restarted at 1 since nothing has been released). Their predecessors (revision 5: `0x2780675B…`, `0xF77063b0…`, `0x5fdbaD4f…`, `0x3616c0b2…`; revision 6 on Eden: `0x45A9Fd2D…`) keep their last state and nothing points at them.

On Eden the routers do not point at the ISM directly but at a routing ISM of ours, `0xD93E3ff7c961BD0668025B39B2685E333b128371` (Hyperlane `AtomicInitDomainRoutingIsm`, Celestia's domain routed to the ISM above), and dispatch through a routing hook of ours, `0xD5850ae1213d0D1f2C24Bbca030FD5f734d8FedC` (Hyperlane `DomainRoutingHook`, Celestia's domain routed to the merkle tree hook). That is the mainnet shape: a token owner points at the routing contracts once, and the TEE ISM behind them is ours to swap. `teeism warp evm` does not know them and re-points the routers straight at the ISM; after it, `set` the routing ISM to the new ISM and point the routers back ([MAINTAIN](deploy/MAINTAIN.md)).

| EVM-origin, `x/teeism` on `teeism-local` | id |
|---|---|
| Sepolia `11155111` | `0x726f757465725f69736d000000000000000000000000002b0000000000000001` |
| Arbitrum `421614` | `0x726f757465725f69736d000000000000000000000000002b0000000000000002` |
| Base `84532` | `0x726f757465725f69736d000000000000000000000000002b0000000000000003` |
| Eden `3735928814` | `0x726f757465725f69736d000000000000000000000000002b0000000000000004` |
| routing ISM over all four | `0x726f757465725f69736d00000000000000000000000000010000000000000005` |

**Tokens.** Synthetic on every EVM chain. EVM to EVM is two hops via Celestia.

| | collateral | synthetic |
|---|---|---|
| TIA | Celestia `0x726f757465725f61707000000000000000000000000000010000000000000000` | Sepolia `0x2022b3702c5ab233ce9Da34197ee9b5942AD11fD`<br>Arbitrum `0x7a09FC80cAFaff73335811B431a41dE3d06d0CC2`<br>Base `0x0068917588ceE1afCe257422A6Ee10eAef445f10`<br>Eden `0x62AD8dEbAE1895aEdB0f4Fc2AcaB1B3FAE67Fe02` |
| teeUSD | none: minted on Celestia, as the synthetic `0x726f757465725f61707000000000000000000000000000020000000000000001` (denom `hyperlane/<that id>`) | Sepolia `0xc381f89241D03C7DD62eFe7E9c1e456Cf9f0904f`<br>Arbitrum `0x316006Ed5A096E1F1Ab32b436696B576A7645F0c`<br>Base `0xB151c781c0DfB74807Aa67b7198eB0a0A55e142A`<br>Eden `0x4592f8D742E8b3881B3B94C00260C95aFcc298c0` |

**Eden.** Evolve-stack; mailbox and hook are ours.

```
chain id     3735928814      ~10 blocks/s
mailbox      0xbFbc80F66CD2E3534779d4952CC669c86F298479
merkle hook  0x7fE7582d7BE76f4053c291C30d38284cb4935A1a   (tree at slot 151)
DA           mocha-5, namespace 0000000000000000000000000000000000005d2e074163aa3b4d9818
sequencer    ed25519 4366433b4309d4f077f0cc1f4370a525736df9a1dc9a205b8d2db1d630b68d51
```

**Base and Arbitrum.** Canonical Hyperlane deployments. Pinned sequencer keys:

```
base       0xb830b99c95Ea32300039624Cb567d324D4b1D83C   SystemConfig 0xf272670eb55e895584501d564AfEB048bEd26194 unsafeBlockSigner
arbitrum   0x9396c22161c821231ad4ae8fcf991b4beee39990   SequencerInbox 0x6c97864CE4bEf387dE0b3310A44230f7E3F1be0D isSequencer
```

**Intel collateral (PCCS).** Ours, Automata's contracts at the same CREATE2 addresses on all four EVM chains (namespace `tee-isms/v2`, owner `0x318d22faa1e0f29eac7ef644a8fac676f6688d1e`):

```
entrypoint   0x1AeAc1f7280fb57c1A2d82bD7f76875a7C32A9d3
router       0x46ccfafa9413dc904eb6dd07e5b3ef1ad6dc8cee
v4 verifier  0xbdde242942c2c6dbd8194235d4011d8296aa6b08
v5 verifier  0xccc63af1c2ed8777c31ed90a1acec8ef1c08e83f
storage      0x7a58de47eb15fa9bcac84e1d917046c3792e85c9
```

The DAOs and helpers: `devnet/.state/deployment.json`, `evm.<chain>.pccs`.

**Host.** `ark` runs everything except the enclaves.

```
:3000  UI, and the only public way to reach the chain (/rpc /rest /api /evm/<chain>/ /tx/<hash>)
:3001  relayer dashboard and API
:3002  gas oracle dashboard and API

teeism-celestia    docker   the chain
mocha-light        docker   celestia-node light node, for Eden
teeism-gateway     docker   UI and proxies
teeism-relayer     systemd  every route, the API, the faucet
teeism-gas-oracle  systemd  paymaster upkeep
```

## Design

- Enclave is stateless; its state lives in the ISM.
- Shared CVM = one identity: any image change re-pins all 8 ISMs. A chain in its own CVM re-pins only its ISMs.
- Each ISM pins a set of 1 to 8 enclaves, any one of which may attest; the state carries the set's digest. Two enclaves keep a route up while one is down, and a rotation pins old and new together so either is accepted while the relayer moves; a batch in flight across a re-pin is refused as a trusted-state mismatch and rebuilt.
- Re-pin in place: `TeeDcapIsm.setEnclaves` (EVM), `tx teeism set-identities` (Celestia). Address, root, height kept; no router moves. Only a `TeeDcapIsm` `VERSION` change replaces an EVM ISM, from its predecessor's last state, and re-points the routers.
- `tx teeism set-verifier-overrides`: trust a rotated Intel root CA. Nothing relaxes Intel's verification.
- `tx teeism update-owner`: hand an ISM to a multisig. EVM controls sit with the Automata owner. See [ADMIN.md](deploy/ADMIN.md).
- Rollups: enclave checks the sequencer signature, takes the signed state root, proves the tree. No re-execution, no L1 wait.
- The enclave attests the origin tree's root, not a list of messages. Each message proves itself at delivery with a merkle branch in its metadata against any root the ISM has attested, so an origin block of any size is one attestation and a message dispatched during an outage stays deliverable.
- Anyone may submit an attestation, so the ISM can move past messages the relayer has not indexed. The relayer tracks the origin height it has accounted for separately from the ISM's, and delivers whatever lies between them.
- Heartbeat: a quiet route attests daily with no new leaves, so its ISM stays near its origin's head. Both ISMs refuse an attestation that leaves the tree root unchanged and moves the origin clock less than 30 minutes.

## Trust

- **Trusted:** Intel TDX, pinned identity and its re-pin key, each ISM's starting state, our Automata contracts (EVM), Base/Arbitrum/Eden sequencer keys.
- **Not trusted:** RPCs, the relayer (can stall, not forge), Phala beyond uptime, enclave inputs.
- **Risks:** a TDX break forges anything. The owner can re-pin any ISM to an enclave of its choice. One EOA owns ISMs, routers and Automata roles. Moves to a multisig before mainnet (`update-owner` on Celestia, `transferOwnership` on EVM).

## Tests

```sh
cd tee-hyperlane && cargo test                        # state proofs, trees, L2 roots, Eden execution
cd tee-hyperlane/contracts && forge test              # TeeDcapIsm.sol
cd cli && cargo test                                  # teeism
cd ../celestia-app-local && go test ./x/teeism/...    # the Celestia verifier
```
