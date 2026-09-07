# Live testnet deployment

Everything below is deployed and reachable. Addresses are stable; vkeys and the enclave
identity move together whenever the enclave image or its compose file changes, because the
identity is measured from exactly those.

## Enclaves — Phala Cloud, node prod9

| | |
|---|---|
| image | `ghcr.io/jonas089/tee-node@sha256:098cc066…` |
| OS | `dstack-0.5.9` (production; `is_dev = false`) |
| instance | `tdx.small`, 1 vCPU / 2 GB / 20 GB |
| cost | $0.0608/hr each, $2.92/day for both |

```
tee-node-ethereum  a1e3cd5d7fd24c2d1dc82237005316e4cd334db0
tee-node-celestia  a545800ddb811ccf2a5c9bcba4e18b28fb402cc0
https://<app-id>-8080.dstack-pha-prod9.phala.network
```

Both run the *same* compose file, so both measure identically. `app-id` and `instance-id`
differ and are deliberately not pinned.

> prod5 cannot host these: its teepod reports `tproxy_base_domain: None`, so the gateway
> never registers an instance and every request terminates TLS then returns empty. Use
> `--node-id 18` (the *teepod* id for prod9), not `--node-id 9`.

## Enclave identity and circuits

```
mr_td          f06dfda6dce1cf904d4e2bab1dc370634cf95cefa2ceb2de2eee127c93826980…
os_image_hash  bd369a8c2f9edb2b52dad48ac8e0b32dde5f1337c423a506b48d07403a7d8033
compose_hash   6d5768a900398b566d58bd0773ca1f9fa4964acbcac2eef690e2e…
identity        3a7485ed3a1510392fc486ded782074b79228beddade0256a17cfc643a76e0d2

state_transition_vkey  0x000418a0d4a0a30e349a683f04e657b05cd8f596366fa6dab3f116f4262c92b8
state_membership_vkey  0x0008514a2af6c5a50da1312a385a3839f453a4fcd847a5f1c997df338296247b
groth16 wrap vk        396 bytes, sha256 a4594c59… (identical to celestia-app v9.0.6's)
```

## Celestia mocha-5

Deployed by us; the chain had no Hyperlane stack at all.

```
mailbox           0x68797065726c616e650000000000000000000000000000000000000000000000
merkle tree hook  0x726f757465725f706f73745f6469737061746368000000030000000000000000
noop hook         0x726f757465725f706f73745f6469737061746368000000000000000000000001
noop ism          0x726f757465725f69736d00000000000000000000000000000000000000000000
TIA collateral    0x726f757465725f61707000000000000000000000000000010000000000000000
local domain      1297040200   (ASCII "MOCH")
```

> `required_hook` is the merkle tree hook; `default_hook` **must** be the noop hook. Setting
> both to the merkle tree hook inserts every message into the tree twice, which is harmless
> for the root but doubles the leaves the relayer has to replay.

## Ethereum Sepolia

```
TeeIsm            0xb9E5E3eb926EA22B951d2fb7392F9F3D6c704054
synthetic TIA     0xFeA14C1444A7a8beAb7122fdE5A168212D7185bE   ism -> TeeIsm
mailbox           0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766   (Hyperlane canonical)
merkle tree hook  0x4917a9746A7B6E0A57159cCb7F5a6744247f2d0d   (branch at slot 103)
SP1 verifier      0x50ACFBEdecf4cbe350E1a86fC6f03a821772f1e5   (v5.0.0 groth16)
```

## Operational notes

**`eth_getProof` needs an archive endpoint.** The coprocessor proves the origin tree twice:
once at the finalized head, once at the ISM's trusted height to get the snapshot to replay
onto. The second is usually more than 128 blocks back, which public RPCs refuse with
"distance to target block exceeds maximum proof window". `https://rpc.sepolia.ethpandaops.io`
and `https://1rpc.io/sepolia` both serve it. A production relayer should cache the tree it
proved last round instead, and only reach back at bootstrap.

**A shared mailbox means shared batches.** Hyperlane's canonical Sepolia mailbox is used by
everyone, so the merkle tree contains other people's messages. The attested batch must
contain *every* leaf in the range or the replay cannot reproduce the branch — so the ISM
authorises those ids too. They are never delivered here, because their destination domain is
not ours. It costs a little state and nothing else.

**Ethereum finality gates the Sepolia -> Celestia direction.** The enclave attests the
*finalized* head, not the head, so a message waits roughly two epochs (~13 minutes) before it
can be attested at all. Celestia finalises in one block, so the other direction does not wait.

## Running

`tee-hyperlane run --config coprocessor.toml` drives every configured route: read the ISM
state, attest, prove, submit, deliver. A tick with nothing to do costs nothing, and a tick
that fails warns and retries — the ISM state on the destination chain is the only progress
marker, so restarting is the same as continuing.

The same stages are also individual subcommands, which is how you step through a route that
is misbehaving. They call the same functions, so the results are identical.

## Running one step by hand

```sh
tee-hyperlane bootstrap-celestia --height <h> --identity-digest <d>   # once, at ISM creation
tee-hyperlane attest-celestia --enclave <url> --trusted-state <hex> \
    --merkle-tree-hook <id> --out attestation.json
tee-hyperlane prove --attestation attestation.json --out proved.json  # ~4.5 min per proof
TEE_ISM=<addr> MAILBOX=<mailbox> EVM_RPC=<rpc> deploy/submit-evm.sh proved.json
```

Ethereum to Celestia:

```sh
tee-hyperlane bootstrap-ethereum --identity-digest <d>                # once, at ISM creation
tee-hyperlane attest-ethereum --enclave <url> --checkpoint <root> \
    --trusted-state <hex> --execution https://rpc.sepolia.ethpandaops.io \
    --out attestation.json
tee-hyperlane prove --attestation attestation.json --out proved.json
CELESTIA_ISM=<id> deploy/submit-celestia.sh proved.json
```
