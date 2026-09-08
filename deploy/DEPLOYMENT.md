# Live testnet deployment

Everything below is deployed and reachable, and nothing from an earlier identity is still in
use. [../docs/verify-deployment.md](../docs/verify-deployment.md) is how to check any of it
without trusting this file.

Addresses are stable; vkeys and the enclave identity move together whenever the enclave image
or its compose file changes, because the identity is measured from exactly those.

## Enclaves - Phala Cloud, node prod9

| | |
|---|---|
| image | `ghcr.io/jonas089/tee-node@sha256:0109b978faed93a63e984d6a1dc1b713bf236e14c6d7413886a04e603b1f2d2f` |
| built by | `nix build .#image`, reproducible |
| OS | `dstack-0.5.9` (production; `is_dev = false`) |
| instance | `tdx.small`, 1 vCPU / 2 GB / 20 GB |
| cost | $0.0608/hr each, $2.92/day for both |

```
tee-node-ethereum  f8da571c89be34182a39099d2c0684f6d46dfd75
tee-node-celestia  d37cfd9c3598f2335cd3c6c4a1f76da46407d421
https://<app-id>-8080.dstack-pha-prod9.phala.network
```

Both run the *same* compose file, so both measure identically. The names say which route uses
which; either enclave could serve either origin. `app-id` and `instance-id` differ and are
deliberately not pinned.

> Deploy with `--node-id 18`. Auto-selection sometimes lands on prod5, whose teepod reports
> `tproxy_base_domain: None`: the CVM runs, the gateway never registers it, and every request
> terminates TLS then returns nothing. Node 26 is prod5; node 18 is prod9.

## Enclave identity and circuits

```
mr_td          f06dfda6dce1cf904d4e2bab1dc370634cf95cefa2ceb2de2eee127c93826980...
os_image_hash  bd369a8c2f9edb2b52dad48ac8e0b32dde5f1337c423a506b48d07403a7d8033
compose_hash   bb35e830a2ec224642e9ae963e964d5941e1d5ae1a1256eca4c9550bb2c42b6d
mr_kms         92a4bf40c88734b0e56f54b09b1f0fe4b8d3e230047e9298f491968ada8dedf8
identity       5d6083a9631b6f75d20489febe15f619bd2d0953f6d9f574713230cf275d6c5a

state_transition_vkey  0x00350b158e64c20dc65eb4eaa2445da8e68264a6ebc092cd9104d9464d1ce458
state_membership_vkey  0x0028f85f3f0a3d431b8b1b7804d1ca0c2024dbb70cb303e043724b63bb257c99
groth16 wrap vk        396 bytes, sha256 a4594c59... (identical to celestia-app v9.0.6's)
```

## Celestia mocha-5

```
mailbox           0x68797065726c616e650000000000000000000000000000000000000000000000
merkle tree hook  0x726f757465725f706f73745f6469737061746368000000030000000000000000  (required_hook)
IGP               0x726f757465725f706f73745f6469737061746368000000040000000000000002  (default_hook)
domain            1297040200

routing ISM       0x726f757465725f69736d0000000000000000000000000001000000000000000c
  11155111  ->    0x726f757465725f69736d000000000000000000000000002a0000000000000009
  421614    ->    0x726f757465725f69736d000000000000000000000000002a000000000000000a
  84532     ->    0x726f757465725f69736d000000000000000000000000002a000000000000000b

TIA  collateral   0x726f757465725f61707000000000000000000000000000010000000000000000
USDC synthetic    0x726f757465725f61707000000000000000000000000000020000000000000001
```

Celestia accepts three origins, so the warp tokens point at the routing ISM rather than at one
ISM. An ISM's state chain is one origin's history, which is why each origin needs its own.

## Ethereum Sepolia

```
TeeIsm            0x6f31D79D898f86a60832Fd1caB31ceC67Bc71Fb6
synthetic TIA     0xFeA14C1444A7a8beAb7122fdE5A168212D7185bE
collateral USDC   0xfb611B6f6CE92033960e99C2D65cee4237e64cDD
mailbox           0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766   (Hyperlane canonical)
merkle tree hook  0x4917a9746A7B6E0A57159cCb7F5a6744247f2d0d   (branch at slot 103)
SP1 verifier      0x50ACFBEdecf4cbe350E1a86fC6f03a821772f1e5   (v5.0.0 groth16)
```

## Arbitrum Sepolia

```
TeeIsm            0x21bdf13D66D3e5F0D4793B64bb4c85034B9EDc88
synthetic TIA     0xFeA14C1444A7a8beAb7122fdE5A168212D7185bE
synthetic USDC    0xb9E5E3eb926EA22B951d2fb7392F9F3D6c704054
mailbox           0x598facE78a4302f11E3de0bee1894Da0b2Cb71F8
merkle tree hook  0xAD34A66Bf6dB18E858F6B686557075568c6E031C   (branch at slot 151)
BoLD rollup       0x042B2E6C5E99d4c521bd49beeD5E99651D9B0Cf4   (pinned in the enclave)
```

## Base Sepolia

```
TeeIsm            0x1D32350f3440BEa7f7E450Aa085f63E0d7E38729
synthetic TIA     0xf4197C55C944987E9b10e09C0A47915211769B78
synthetic USDC    0x0ee6374a92ba4E11F920A23c6dd271b594D69A9B
mailbox           0x6966b0E55883d49BFB24539356a2f8A673E02039
merkle tree hook  0x86fb9F1c124fB20ff130C41a79a432F770f67AFD   (branch at slot 151)
anchor registry   0x2fF5cC82dBf333Ea30D8ee462178ab1707315355   (pinned in the enclave)
```

The SP1 v5 verifier is at the same address with byte-identical code on all three EVM chains,
checked rather than assumed.

An L2's anchor contract and its storage layout are compiled into the enclave, not taken from
the request. A proof against a caller-named contract proves nothing: anyone can deploy a
contract whose storage mimics a rollup and prove it honestly against the real L1 state root.

## Gas

An IGP on each side, kept current hourly by `crates/gas-oracle`.

```
Celestia IGP       0x726f757465725f706f73745f6469737061746368000000040000000000000002
Sepolia   IGP      0x48b1BF6CC2e45Ca52947E95Bb216C2eBdCB19c49   oracle 0x225B8488242c90085B7A8Ea33Ce8e39Ae9f79722
Arbitrum  IGP      0x0ee6374a92ba4E11F920A23c6dd271b594D69A9B   oracle 0xfA8036Cb092079B095ed60750d7b39c3C220F288
Base      IGP      0x5591613C85E9bC95104980d4485c958ee80f6F76   oracle 0x7A7042C8784700618be87Aac7F9336620e216Bb9
```

Each EVM router's hook is a `TreeAndPaymasterHook`, not the IGP directly:

```
Sepolia   0x0315dd118C2ba17aC74E708C051041fDC8920697
Arbitrum  0x545ACFF483381f89123c2F520D1B4f7d0DCe60A1
Base      0x225B8488242c90085B7A8Ea33Ce8e39Ae9f79722
```

A router picks exactly *one* post-dispatch hook, and this bridge needs two: the merkle tree
hook, or the message is never inserted and can never be attested, and the paymaster. Pointing
a router straight at the IGP costs it the first, silently - the transfer succeeds and the
message is unprovable forever. Verified by dispatching and watching the hook's `count()`
advance.

On Celestia no aggregation is needed: `required_hook` is already the merkle tree hook, so
`default_hook` is set to the IGP and both run.

Falling back to the mailbox default is not an option either - Hyperlane's Sepolia default
quotes **0.009 ETH** for Celestia's domain, about $22 a transfer, because the domain is not
in its oracle. Ours quotes $0.0014. Deployment and the arithmetic are in
[server/README.md](server/README.md).

## Operational notes

**`eth_getProof` needs an archive endpoint.** The coprocessor proves the origin tree twice:
once at the finalized head, once at the ISM's trusted height to get the snapshot to replay
onto. The second is usually more than 128 blocks back, which public RPCs refuse with
"distance to target block exceeds maximum proof window". `https://rpc.sepolia.ethpandaops.io`
and `https://1rpc.io/sepolia` both serve it. A production relayer should cache the tree it
proved last round instead, and only reach back at bootstrap.

**A shared mailbox means shared batches.** Hyperlane's canonical Sepolia mailbox is used by
everyone, so the merkle tree contains other people's messages. The attested batch must
contain *every* leaf in the range or the replay cannot reproduce the branch - so the ISM
authorises those ids too. They are never delivered here, because their destination domain is
not ours. It costs a little state and nothing else.

**Arbitrum Sepolia is BoLD, and the obvious rollup address is the wrong one.** The canonical
`0xd808…81C8` is the deprecated pre-BoLD contract and has created no node in over eleven days;
building against its `getNode`/`confirmData` layout produces a root that never advances. The
live rollup is `inbox.bridge().rollup()` = `0x042B2E6C…0Cf4`, which stores an assertion hash.

**Ethereum finality gates the Sepolia -> Celestia direction.** The enclave attests the
*finalized* head, not the head, so a message waits roughly two epochs (~13 minutes) before it
can be attested at all. Celestia finalises in one block, so the other direction does not wait.

## Running

`tee-hyperlane run --config coprocessor.toml` drives every configured route: read the ISM
state, attest, prove, submit, deliver. A tick with nothing to do costs nothing, and a tick
that fails warns and retries - the ISM state on the destination chain is the only progress
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
