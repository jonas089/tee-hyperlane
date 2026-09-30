# TEE ISMs

Hyperlane bridging between Celestia and EVM chains, where messages are authorised by a **light
client running inside a TDX enclave** instead of a validator multisig. The destination
verifies the enclave's TDX quote directly. There is no zero-knowledge proof anywhere.

| doc | for |
|---|---|
| [deploy/DEPLOY.md](deploy/DEPLOY.md) | standing up the whole bridge, step by step |
| [deploy/MAINTAIN.md](deploy/MAINTAIN.md) | the monthly job, rolling out new code, diagnosing a route |
| [deploy/INTERACT.md](deploy/INTERACT.md) | wallets, sending, checking arrival, latency and cost |

## How it works

1. An enclave (a Phala TDX VM) verifies the origin chain, proves which messages were sent, and
   signs that in a TDX quote.
2. The relayer submits the quote to the destination's ISM, which checks it came from our
   enclave, then delivers the messages.

The enclave stores nothing; its state lives in the ISM on chain.

## Layout

```
tee-hyperlane/crates/tee-node/          the enclave
tee-hyperlane/crates/tee-coprocessor/   the service: every route, the API, the faucet
tee-hyperlane/contracts/                TeeDcapIsm.sol
bridge-app/                             React UI, MetaMask + Keplr
devnet/                                 scripts that deploy everything, and the gateway
deploy/                                 the guides, the measured compose files, systemd units
```

Both crates have one file per chain: everything about Base is `tee-node/src/ethereum/base.rs`
(how the enclave verifies it) and `tee-coprocessor/src/origin/ethereum/base.rs` (how its proofs
are fetched). The coprocessor splits into `origin/` and `destination/`, each with its trait in
`mod.rs` and one file per implementation.

## Deployments

Check any value here yourself with `FAMILY=<family> deploy/verify-digest.sh <app-id>`.

**Enclaves.** Phala Cloud `prod9`, `tdx.small`, OS `dstack-0.5.9`, image
`ghcr.io/jonas089/tee-node`.

| family | app id | attests | identity |
|---|---|---|---|
| `celestia` | `e6a9dc0dcf0b905cbcb561562cbcd6c814adb126` | Celestia | `0xf3640dbb03c52fa649ae03431a8511e431f78f00d9b8cb622dcd21c9716a82b5` |
| `ethereum` | `6479fb4b957ce078ede97d5999232210eea646ef` | Sepolia, Arbitrum, Base | `0xbc84fb7140c6bd6e432bcb13b18f2ab5065068803582cf075e688df9d0878b62` |
| `evolve` | `c3789ecebee1956df200741e968814f4608847bb` | Eden | `0x9bb4f75ab3efe61eafb15bb261d5fccfce81af9252da89e37b571ca3daec2e78` |

```
mr_td          f06dfda6dce1cf904d4e2bab1dc370634cf95cefa2ceb2de2eee127c9382698090d7a4a13e14c536ec6c9c3c8fa87077
os_image_hash  bd369a8c2f9edb2b52dad48ac8e0b32dde5f1337c423a506b48d07403a7d8033
mr_kms         92a4bf40c88734b0e56f54b09b1f0fe4b8d3e230047e9298f491968ada8dedf8

                celestia                                                           ethereum                                                           evolve
compose_hash    37d2642c1dfdff01c93e8b5c931d0ce6e681d88cb2403c2ff98eb84ed7b246a2   8f294a664953f6db736f2a944b2acbc4d0ffd89a0e8e092cf071ab969501ba43   c72ffa5756938376495269366d58a89fcecb3646ac5cfde0bfdff4b7b92647fb
measurements    0x85db587bf21fad1c4648f368d4fbc1cb95222765274c2ee643e717ef142ec921 0xcd9f2141389162348a791c449e2b63bca2170b0d71b986cb2f1348ca052437d2 0xece515445d5d05f08f2590936d884776a5dd02244e555cefedb14b4f928b9c84
```

The EVM ISMs pin `measurements`; the Celestia ISMs pin the fields behind `identity`.

**Chain.** Our own devnet, not mocha, with pruning off.

```
chain id   teeism-local
domain     1297040299
mailbox    0x68797065726c616e650000000000000000000000000000000000000000000000
merkle     0x726f757465725f706f73745f6469737061746368000000030000000000000000
igp        0x726f757465725f706f73745f6469737061746368000000040000000000000002
```

**ISMs.** One per origin domain, so eight.

| Celestia-origin, `TeeDcapIsm` on | address |
|---|---|
| Ethereum Sepolia | `0x865264E0ae7b173943e34c493E7851B895F37E3F` |
| Arbitrum Sepolia | `0x39b7Cefe6523c474A31652cc297bA2b06ac53AB3` |
| Base Sepolia | `0x94cC258254B4e9ef6675E6f3a2Ee023D3D21bFcC` |
| Eden | `0xe686F221B7A2B641574340F43Ad5cD59707791Ca` |

| EVM-origin, `x/teeism` on `teeism-local` | id |
|---|---|
| Sepolia `11155111` | `0x726f757465725f69736d000000000000000000000000002b000000000000003c` |
| Arbitrum `421614` | `0x726f757465725f69736d000000000000000000000000002b000000000000003d` |
| Base `84532` | `0x726f757465725f69736d000000000000000000000000002b000000000000003e` |
| Eden `3735928814` | `0x726f757465725f69736d000000000000000000000000002b000000000000003f` |
| routing ISM over all four | `0x726f757465725f69736d00000000000000000000000000010000000000000035` |

**Tokens.** Each asset has its collateral on its home chain and is a synthetic elsewhere.

| | collateral | synthetic |
|---|---|---|
| TIA | Celestia `0x726f757465725f61707000000000000000000000000000010000000000000000` | Sepolia `0x9822eE81C82138F88D759faef1AC168aDfEe1467`<br>Arbitrum `0x41f992F671D04c5C26350E64FFA3E1D90bc33bcB`<br>Base `0xF50470146B36c638b981e437AB37DfEd9a02FAb3`<br>Eden `0xD2babc9BE1055551b7AB98c440222862a1646158` |
| USDC | Sepolia `0xfb611B6f6CE92033960e99C2D65cee4237e64cDD` (Circle's `0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238`) | Celestia `0x726f757465725f61707000000000000000000000000000020000000000000001`<br>Arbitrum `0x8C87fd144006C651430450df8b61A15EeB3FF436`<br>Base `0x285b590ee43A1374AA131e7390D0CA687Be43DF9`<br>Eden `0xc09fbf8F17E96ce746D39f9d11a9dD1813F2d220` |

Every route runs through Celestia, so EVM to EVM is two hops.

**Eden.** An evolve-stack chain. It had no Hyperlane deployment, so the mailbox and hook are
ours.

```
chain id     3735928814      ~10 blocks/s
mailbox      0x1D32350f3440BEa7f7E450Aa085f63E0d7E38729
merkle hook  0xCfBE7016D123d52A7Db4fc7D087cCb5421dbF8db   (tree at slot 151)
DA           mocha-5, namespace 0000000000000000000000000000000000005d2e074163aa3b4d9818
sequencer    ed25519 4366433b4309d4f077f0cc1f4370a525736df9a1dc9a205b8d2db1d630b68d51
```

**Host.** One server, `ark`, runs everything except the enclaves.

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

- **One enclave per origin family.** A code change to one family only replaces that family's
  ISMs: celestia → 4, ethereum → 3, evolve → 1. Shared code or `Cargo.lock` → all 8.
- **Arbitrum and Base are slow on purpose.** A message waits until its rollup block is
  confirmed on L1: ~1h40m for Arbitrum, 5 days for Base.
- **Eden is re-executed.** The enclave replays Eden's blocks with ev-reth's own executor and
  must reach the root the sequencer signed. The sequencer can reorder or stop, not invent state.

## Trust

- **Trusted:** Intel TDX, the pinned enclave identity, each ISM's starting state, and on EVM
  chains our Automata contracts.
- **Not trusted:** RPCs, the relayer (it can stall, not forge), Phala beyond uptime, anything
  sent to the enclave.
- **Risks:** a TDX break forges anything; one EOA owns the ISMs, routers and Automata roles.

## Tests

```sh
cd tee-hyperlane && cargo test                        # state proofs, trees, L2 roots, Eden execution
cd tee-hyperlane/contracts && forge test              # TeeDcapIsm.sol
cd ../celestia-app-local && go test ./x/teeism/...    # the Celestia verifier
```
