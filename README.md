# TEE ISMs

Hyperlane bridging between Celestia and EVM chains, where messages are authorised by a **TDX
enclave that verifies the origin chain** instead of a validator multisig: a light client for
Celestia and Ethereum, the sequencer's signature for the rollups. The destination verifies the
enclave's TDX quote directly. There is no zero-knowledge proof anywhere.

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

Both crates have one file per chain, grouped the same way: `l1/` for chains with their own
consensus, verified by a light client (Celestia, Ethereum), and `l2/` for rollups, verified by
their sequencer's signature (Base, Arbitrum, Eden). Everything about Base is
`tee-node/src/chains/l2/base.rs` (how the enclave verifies it) and
`tee-coprocessor/src/origin/l2/base.rs` (how its blocks and proofs are fetched). The coprocessor
splits into `origin/` and `destination/`, each with its trait in `mod.rs`.

## Deployments

Check any value here yourself with `TARGET=all deploy/verify-digest.sh <app-id>`.

**Enclaves.** Phala Cloud `prod9`, `tdx.small`, OS `dstack-0.5.9`, image
`ghcr.io/jonas089/tee-node`. One image per origin, all five in one CVM
(`deploy/docker-compose.all.yml`), each on its own port. They share the CVM's identity. A chain
can also run alone in its own CVM with `30-enclave-up.sh --<chain>`, and then has its own.

| enclave | port | attests |
|---|---|---|
| `celestia` | 8080 | Celestia |
| `ethereum` | 8081 | Sepolia |
| `base` | 8082 | Base Sepolia |
| `arbitrum` | 8083 | Arbitrum Sepolia |
| `eden` | 8084 | Eden |

The app id, identity and measurements below are written at the rollout of the shared CVM;
until then the three per-family CVMs (`e6a9dc0d…`, `6479fb4b…`, `c3789ece…`) still serve.

```
app id         set at rollout
identity       set at rollout
compose_hash   set at rollout
measurements   set at rollout
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

**Tokens.** Both assets start on Celestia and are synthetic on every EVM chain.

| | collateral | synthetic |
|---|---|---|
| TIA | Celestia `0x726f757465725f61707000000000000000000000000000010000000000000000` | Sepolia `0x9822eE81C82138F88D759faef1AC168aDfEe1467`<br>Arbitrum `0x41f992F671D04c5C26350E64FFA3E1D90bc33bcB`<br>Base `0xF50470146B36c638b981e437AB37DfEd9a02FAb3`<br>Eden `0xD2babc9BE1055551b7AB98c440222862a1646158` |
| teeUSD | none: a fixed 1B minted once on Celestia, as the synthetic `0x726f757465725f61707000000000000000000000000000020000000000000002` (denom `hyperlane/<that id>`) | Sepolia `0x1be2055d49c0a350C137605a62861f58Aa750297`<br>Arbitrum `0xF699EB10087A1e01993ed628C198fe6e231830eD`<br>Base `0xa8cc6ec4855b1415c03f3aE51953a3643f7991a5`<br>Eden `0xa4697140E90F39D94F3C64D40175FA6F207bc8EC` |

Every route runs through Celestia, so EVM to EVM is two hops.

**Pools.** One Uniswap v3 pool of synthetic TIA against synthetic teeUSD per chain, fee 0.3%,
full range, seeded with 1M TIA and 500k teeUSD (TIA at 0.5 teeUSD). The deployer
`0x318d22faa1e0f29eac7Ef644A8FaC676F6688d1e` owns each position.

| chain | pool |
|---|---|
| Sepolia | `0x52E470FBe0B120FAcb9a8B6Efbfe58a6263d909D` |
| Base | `0x6E828f3B976d456fe673d26bC0CBEA45838dD018` |
| Arbitrum | `0xe4d1fc54f8B8AfFcD7e2ffF7dC00d000A5CC4FbF` |

**Eden.** An evolve-stack chain. It had no Hyperlane deployment, so the mailbox and hook are
ours.

```
chain id     3735928814      ~10 blocks/s
mailbox      0x1D32350f3440BEa7f7E450Aa085f63E0d7E38729
merkle hook  0xCfBE7016D123d52A7Db4fc7D087cCb5421dbF8db   (tree at slot 151)
DA           mocha-5, namespace 0000000000000000000000000000000000005d2e074163aa3b4d9818
sequencer    ed25519 4366433b4309d4f077f0cc1f4370a525736df9a1dc9a205b8d2db1d630b68d51
```

**Base and Arbitrum.** The canonical Hyperlane deployments. The sequencer keys the enclave pins:

```
base       0xb830b99c95Ea32300039624Cb567d324D4b1D83C   SystemConfig 0xf272670eb55e895584501d564AfEB048bEd26194 unsafeBlockSigner
arbitrum   0x9396c22161c821231ad4ae8fcf991b4beee39990   SequencerInbox 0x6c97864CE4bEf387dE0b3310A44230f7E3F1be0D isSequencer
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

- **One image per origin, one CVM for all of them.** The shared CVM has one identity, so a
  code change to any image replaces all 8 ISMs. A chain moved to its own CVM
  (`30-enclave-up.sh --<chain>`) gets its own identity, and a change to it then replaces only
  the ISMs that chain attests.
- **Rollups are attested from what their sequencer signed.** Base's sequencer signs every
  block on the OP Stack p2p network, Arbitrum's every feed message (which commits to the block
  hash), Eden's every header it posts to Celestia. The enclave checks the signature against the
  pinned key, takes the state root from the signed block, and proves the Hyperlane tree under
  it. No re-execution, and no waiting for L1: a transfer is attested seconds after its block.

## Trust

- **Trusted:** Intel TDX, the pinned enclave identity, each ISM's starting state, on EVM
  chains our Automata contracts, and the Base, Arbitrum and Eden sequencer keys: a sequencer
  that signs a wrong state root, or a block it later reorgs away, is believed.
- **Not trusted:** RPCs, the relayer (it can stall, not forge), Phala beyond uptime, anything
  sent to the enclave.
- **Risks:** a TDX break forges anything; one EOA owns the ISMs, routers and Automata roles.

## Tests

```sh
cd tee-hyperlane && cargo test                        # state proofs, trees, L2 roots, Eden execution
cd tee-hyperlane/contracts && forge test              # TeeDcapIsm.sol
cd ../celestia-app-local && go test ./x/teeism/...    # the Celestia verifier
```
