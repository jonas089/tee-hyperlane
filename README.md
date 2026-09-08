# TEE ISMs

Hyperlane bridging between Celestia and (EVM) Chains, where messages are
authorised by a **light client running inside a TDX enclave**, not by a validator multisig.
Two enclaves cover four networks.

## How it works

An enclave verifies an origin chain's consensus, derives its state root, proves the origin
Hyperlane merkle tree under that root, and confirms a claimed batch of message ids is exactly
the new leaves. It signs all of that into one TDX quote.

That single quote is then proved twice on local CPU, once under each of two SP1 programs,
because Celestia's `x/zkism` decodes the two transactions with two different decoders and no
single blob satisfies both. Both proofs land on the destination chain, which advances its
trusted state and authorises the batch.

```
enclave (Phala CVM, stateless)          coprocessor (untrusted, local CPU)
  verify consensus                        prove attestation  x2   (~4 min each)
  derive state root                       updateState(...)
  prove Hyperlane tree                    submitMessages(...)
  attest via dstack GetQuote              process(message)
```

The enclave keeps nothing. Its light-client state lives in the ISM's `state` field on the
destination chain, so the chain is the light client's database and a restarted enclave loses
nothing.

## Layout

```
tee-circuit/     enclave identity check and the two SP1 programs
tee-hyperlane/   the enclave, the coprocessor, TeeIsm.sol, the CLI
bridge-app/      React UI, MetaMask + Keplr
deploy/          compose file, submit scripts, systemd and nginx
docs/            integration guide
```

## Docs

- [docs/integrations.md](docs/integrations.md) - adding a chain: EVM rollups, evolve-stack,
  and entirely new chains like Solana.
- [deploy/DEPLOYMENT.md](deploy/DEPLOYMENT.md) - live addresses and the deployment footguns.
- [deploy/E2E.md](deploy/E2E.md) - the four testnet transfers, with what each cost.
- [deploy/server/README.md](deploy/server/README.md) - running the coprocessor and UI on a box.
- [bridge-app/README.md](bridge-app/README.md) - the UI.
- [docs/verify-deployment.md](docs/verify-deployment.md) - checking that what is deployed is
  what is in this repo, without taking its word for it.
- [docs/redeploy.md](docs/redeploy.md) - replacing an enclave.

See [docs/verify-deployment.md](docs/verify-deployment.md) to check any of the values
below yourself, without trusting this file.

## Deployments

Enclaves - Phala Cloud `prod9`, image `ghcr.io/jonas089/tee-node`, OS `dstack-0.5.9`
(production), `tdx.small`. $0.0608/hr each.

| node | app id |
|---|---|
| `tee-node-ethereum` | `53989de9c1b33c19f108443e293689b0276b0dcd` |
| `tee-node-celestia` | `9e28bfa7c27a463f2d420e3fcbe269fccb231d57` |

Reach them at `https://<app-id>-8080.dstack-pha-prod9.phala.network`.

The pinned identity constrains the **OS image, the container image and the KMS** - never the
app id or instance id. That is deliberate: instances get replaced and providers may change,
and an identity tied to one instance would have to be re-pinned every time. Both nodes above
were deployed independently and produce byte-identical measurements.

The image is built with Nix, so its digest is reproducible rather than something to take on
trust. [docs/verify-deployment.md](docs/verify-deployment.md) is how to check it yourself.

```
image          ghcr.io/jonas089/tee-node@sha256:c092d13bb58e7342db73c0459e3592cbc71e4aa34147712019a93fc0baa20a36
mr_td          f06dfda6dce1cf904d4e2bab1dc370634cf95cefa2ceb2de2eee127c93826980...
os_image_hash  bd369a8c2f9edb2b52dad48ac8e0b32dde5f1337c423a506b48d07403a7d8033
compose_hash   6650012606d4061e397c35ac8a835fdc2fe00fe13645673beee575e0bfbd5ce7
mr_kms         92a4bf40c88734b0e56f54b09b1f0fe4b8d3e230047e9298f491968ada8dedf8
identity       1f59fa2255b98994abaf49f45bc2d95bd59789560edb9263f7e90976aa1c65f5

state_transition_vkey  0x00bf2e770c4110122d5f716333961d2fa7be3924d42e13f202a6fac70075b267
state_membership_vkey  0x009ff5ec25d15fd0b010bc9586fb6980c47d905f6449fc8c1e8635c1db9abed0
```

### ISMs

Every ISM below was created against the vkeys above. Nothing from an earlier identity is in
use: the routers all point here, and the older ISMs are unreachable rather than merely unused.

| chain | ISM | verifies messages from |
|---|---|---|
| Sepolia | `0xcd08642072d2B10fB6cD2E4C6d27b13027031bFf` | Celestia |
| Arbitrum Sepolia | `0xd2b98Cc846c8E07F212A93515ee68A601B6766f6` | Celestia |
| Base Sepolia | `0xc09fbf8F17E96ce746D39f9d11a9dD1813F2d220` | Celestia |
| Celestia mocha-5 | `0x726f757465725f69736d0000000000000000000000000001000000000000000c` | routes by origin |

Celestia accepts three origins, so its warp tokens point at a routing ISM rather than at one
ISM directly. Each origin gets its own `x/zkism` behind it, because an ISM's state chain is
one origin's history and cannot be shared:

| origin | ISM |
|---|---|
| Sepolia (11155111) | `0x726f757465725f69736d000000000000000000000000002a000000000000000e` |
| Arbitrum (421614) | `0x726f757465725f69736d000000000000000000000000002a000000000000000f` |
| Base (84532) | `0x726f757465725f69736d000000000000000000000000002a0000000000000010` |

The EVM ISMs are `contracts/src/TeeIsm.sol`, a port of `x/zkism`, verifying against SP1's v5
Groth16 verifier `0x50ACFBEdecf4cbe350E1a86fC6f03a821772f1e5` - the same address with
byte-identical code on all three EVM testnets, checked rather than assumed.

### Tokens

| route | Celestia mocha-5 | EVM side |
|---|---|---|
| TIA | collateral `0x726f757465725f61707000000000000000000000000000010000000000000000` | synthetic `0xFeA14C1444A7a8beAb7122fdE5A168212D7185bE` (Sepolia)<br>synthetic `0xFeA14C1444A7a8beAb7122fdE5A168212D7185bE` (Arbitrum Sepolia)<br>synthetic `0xf4197C55C944987E9b10e09C0A47915211769B78` (Base Sepolia) |
| USDC | synthetic `0x726f757465725f61707000000000000000000000000000020000000000000001` | collateral `0xfb611B6f6CE92033960e99C2D65cee4237e64cDD` (Sepolia)<br>synthetic `0xb9E5E3eb926EA22B951d2fb7392F9F3D6c704054` (Arbitrum Sepolia)<br>synthetic `0x0ee6374a92ba4E11F920A23c6dd271b594D69A9B` (Base Sepolia) |

### Gas

Fees are quoted by an IGP on each side and kept current by `crates/gas-oracle`, which reads
gas and token prices hourly. On the EVM side the beneficiary is
`0x318d22faa1e0f29eac7Ef644A8FaC676F6688d1e`; on Celestia they accrue in `utia` to the IGP
owner, because an EVM address cannot hold `utia`.

| chain | paymaster | gas oracle |
|---|---|---|
| Celestia mocha-5 | `0x726f757465725f706f73745f6469737061746368000000040000000000000002` | in-module |
| Sepolia | `0x48b1BF6CC2e45Ca52947E95Bb216C2eBdCB19c49` | `0x225B8488242c90085B7A8Ea33Ce8e39Ae9f79722` |
| Arbitrum Sepolia | `0x0ee6374a92ba4E11F920A23c6dd271b594D69A9B` | `0xfA8036Cb092079B095ed60750d7b39c3C220F288` |
| Base Sepolia | `0x5591613C85E9bC95104980d4485c958ee80f6F76` | `0x7A7042C8784700618be87Aac7F9336620e216Bb9` |

### Services

One VPS runs everything that is not an enclave.

```
:3000  bridge UI
:3001  relayer dashboard and API
:3002  gas oracle dashboard and API
```

See [deploy/server/README.md](deploy/server/README.md) to stand it up, and
[docs/redeploy.md](docs/redeploy.md) to replace an enclave without the system going stale.

Celestia Hyperlane core, deployed by us because mocha-5 had none:

```
mailbox           0x68797065726c616e650000000000000000000000000000000000000000000000
merkle tree hook  0x726f757465725f706f73745f6469737061746368000000030000000000000000
domain            1297040200
```

Full detail, including deployment footguns, in [deploy/DEPLOYMENT.md](deploy/DEPLOYMENT.md).

## Run the tests

```sh
cd tee-circuit   && cargo test          # attestation, identity, x/zkism byte compatibility
cd tee-hyperlane && cargo test          # state proofs, merkle tree, L2 roots
cd tee-hyperlane/contracts && forge test # TeeIsm.sol
```

Much of the suite runs against live chains: Hyperlane's Sepolia merkle tree hook, a Celestia
mocha-5 store proof, a Base Sepolia output root and an Arbitrum Sepolia header are all
checked-in fixtures captured from those chains.

## Build the circuits

```sh
cd tee-circuit
cargo run -p circuit-tool -- build   # compile both guests with SP1 v5 flags
cargo run -p circuit-tool -- vkeys   # the three values an ISM is created with
cargo run -p circuit-tool -- bench --prove   # measure real proving cost here
```

## Deploy

Bootstrapping has one ordering constraint: the enclave identity can only be pinned after an
enclave exists.

1. Build and publish the `tee-node` image; pin its digest in `docker-compose.yml`.
2. Deploy two CVMs (`tdx.small` is enough - the enclave is a verifier, not a prover).
3. `GET /policy` on one of them, write the measurements into
   `tee-circuit/tee-attestation/enclave-identity.toml`, set
   `require_enclave = true`, rebuild the circuits.
4. Deploy Hyperlane core on Mocha, then the warp routes.
5. Deploy `TeeIsm.sol` on Sepolia and point the warp routers at it.
6. `tee-hyperlane run`.

Until step 3, `require_enclave = false` builds a circuit that accepts any genuine non-debug
TDX enclave on an acceptable TCB level. That is enough to develop and test against, and it is
not silent: such a build warns at compile time and produces a distinct
`ANY-DEVELOPMENT-ONLY` identity digest that is visible in the ISM state on chain.

Two things bite here, both recorded in `deploy/DEPLOYMENT.md`: `phala deploy` picks a *dev*
OS image unless you pass `--image`, and dev images allow SSH into the CVM. And a Phala node
whose teepod reports no gateway domain will accept TLS and then answer nothing, however
healthy the container is.

## Run the bridge

```sh
tee-hyperlane run   --config coprocessor.toml      # attest, prove, relay, every route
tee-hyperlane serve --proof-dir /var/lib/...       # attestations for the UI
```

Needs `cast` and `celestia-appd` on PATH: the relayer shells out to them to sign rather than
reimplementing two transaction formats.

## Send and check a transfer

```sh
tee-hyperlane send --route tia-mocha-to-sepolia --token TIA --amount 1000000 --to 0x...
tee-hyperlane verify --message-id 0x...
tee-hyperlane status
```

## Measured cost

On an M3 Max (16 core, 64 GB), against a genuine TDX quote:

```
DCAP verification + event-log replay   3.79M cycles
groth16 proof, SP1_PROVER=cpu          ~280 s
proof size                             260 bytes   (SP1 v5, what x/zkism expects)
```

Two proofs per batch, so roughly **9-10 minutes of local CPU per batch per direction**, plus
origin finality: near-zero for Celestia, ~13 minutes for Ethereum's finalized beacon head. A
batch carries as many messages as were dispatched since the last one, so the cost is per
batch and not per message.

The enclaves themselves are cheap because they prove nothing: 2 x `tdx.small` is $2.92/day.

## What is trusted

Intel TDX and the DCAP PKI; the pinned enclave measurements; the ISM's genesis state, which
names the light-client checkpoint and is public at creation; the two SP1 vkeys; SP1's Groth16
wrap key.

Not trusted: every RPC, the coprocessor, the relayer, Phala as operator, and the host clock -
which is bounded to the attested chain head's timestamp, so a rewound clock cannot revive a
TCB level Intel has revoked.
