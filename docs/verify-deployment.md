# Verifying what is running

Four things are worth checking independently, and none of them requires trusting this repo's
word for it.

## 1. The enclaves run the image the circuits pin

The image is built with Nix, so the digest is reproducible: build it yourself and compare. A
`docker build` digest could only ever be taken on trust, because it bakes in whatever the
builder's machine happened to have.

```sh
nix build .#image                                   # ~35 min cold, from a clean checkout
tar -xOf result manifest.json | jq -r '.[0].Config' # the image config digest
```

That must print:

```
8090132e21034d204b3ea9ecea7797d572d5aff558383e1a1b8c2ca1ac4b00fa.json
```

**Two different digests are involved here and confusing them wastes an afternoon.** The
number above is the *config* digest, which is content-addressed over the image itself. What
`deploy/docker-compose.yml` pins is the *manifest* digest,
`sha256:77283ad03dd5f2dfbbbb718e4b08f63397ce829c48a4e3cc7b3635d18fe3e0d8`, which covers the
manifest document the registry stores. They are not equal, and a `docker inspect --format
'{{.Id}}'` will report one or the other depending on whether your Docker uses the containerd
image store. Compare the config digest, which does not vary by client:

```sh
docker manifest inspect \
  ghcr.io/jonas089/tee-node@sha256:77283ad03dd5f2dfbbbb718e4b08f63397ce829c48a4e3cc7b3635d18fe3e0d8 \
  | jq -r .config.digest
```

To confirm the build is reproducible rather than merely repeatable on one machine:

```sh
nix build .#image --rebuild     # rebuilds and diffs; exit 0 means bit-identical
```

That currently passes.

### What the image does and does not depend on

The Nix source filter is deliberately narrow: `tee-hyperlane/{Cargo.toml,Cargo.lock,
rust-toolchain}`, `crates/hyperlane-types`, `crates/tee-node`, `tee-circuit/Cargo.toml` and
`tee-circuit/tee-attestation`, minus every `tests/` and `testdata/` directory. Editing the
coprocessor, the gas oracle, circuit-tool or a test therefore cannot move the image digest -
which matters, because a moved digest means a new compose hash, a new identity, new vkeys and
six new ISMs.

`enclave-identity.toml` is excluded for a sharper reason. It carries the compose hash, and the
compose hash covers the image digest, so leaving it in the source would make the digest depend
on a value derived from the digest. The build substitutes an all-zero placeholder; the enclave
reads none of those constants, because the policy is enforced in the SP1 guests. You can check
both claims:

```sh
nix eval --raw .#packages.x86_64-linux.image.drvPath   # note it
echo x >> tee-hyperlane/crates/tee-coprocessor/src/commands/mod.rs
nix eval --raw .#packages.x86_64-linux.image.drvPath   # unchanged
echo '# x' >> tee-circuit/tee-attestation/enclave-identity.toml
nix eval --raw .#packages.x86_64-linux.image.drvPath   # unchanged
echo '// x' >> tee-hyperlane/crates/tee-node/src/attest.rs
nix eval --raw .#packages.x86_64-linux.image.drvPath   # changed
```

## 2. The running enclaves measure to the pinned identity

`compose-hash` lands in the enclave's RTMR3, and the circuits pin it. It is *not* the sha256
of `docker-compose.yml`: dstack hashes its own app-compose document, which wraps that file. So
the check is not "hash the file" but "ask a live enclave what it measured and compare against
what is pinned", which closes the same loop without reimplementing their encoding.

```sh
cd tee-circuit
for app in 16882cc467b8d243f0a98ba686d214b910902e02 f6231f5329c6c70a8236045504ab28a9b7e782bb; do
  cargo run -q -p circuit-tool -- identity \
    --url https://$app-8080.dstack-pha-prod9.phala.network | grep -v '^#' > /tmp/live-$app
done
diff /tmp/live-16882cc467b8d243f0a98ba686d214b910902e02 \
     /tmp/live-f6231f5329c6c70a8236045504ab28a9b7e782bb
diff /tmp/live-16882cc467b8d243f0a98ba686d214b910902e02 \
     <(grep -v '^#' tee-attestation/enclave-identity.toml)
```

Both diffs are empty today. The values are:

```
mr_td         f06dfda6dce1cf904d4e2bab1dc370634cf95cefa2ceb2de2eee127c93826980…
os_image_hash bd369a8c2f9edb2b52dad48ac8e0b32dde5f1337c423a506b48d07403a7d8033
compose_hash  47194353c4d3916c9127cf76b56b4fc37c89f86ae9e2c804b06b6d323727cdb3
mr_kms        92a4bf40c88734b0e56f54b09b1f0fe4b8d3e230047e9298f491968ada8dedf8
```

`app-id` and `instance-id` are deliberately *not* pinned. They differ per CVM, while
`compose-hash` already fixes the code, which is what lets one identity cover both enclaves and
lets an enclave be replaced without touching an ISM.

**`os_image_hash` must be `bd369a8c…`.** That is the production dstack OS. If the Phala CLI
finds an SSH public key on the machine you deploy from, it silently provisions
`dstack-dev-0.5.9` instead, whose `os_image_hash` is `de9c74f0…` and which permits shell
access into the CVM. Deploy with `--image dstack-0.5.9 --no-dev-os` and check this value.

**Both enclaves must be deployed fresh, never with `phala cvms upgrade`.** The app-compose
document contains a `name` field. A fresh deploy leaves it empty; an upgrade rewrites it to
`app_<app_id>`, which is per-instance, so upgrading gives the two enclaves different compose
hashes and no single identity can cover them. This is not recoverable by updating in place,
because the field derives from the app id.

## 3. The ISMs pin the vkeys those circuits produce

```sh
cd tee-circuit && cargo run -p circuit-tool -- vkeys
```

```
tee-state-transition  0x00e5e40e1719333e8cad9517eb6c85ab1493318afaa8f49c4e857db125f357f5
tee-state-membership  0x0063a92cdb6d180669c2534e2209a4c7cd77ed2b73c732e805e3f92d930e6df3
identity digest       4d9d26b8b30cd1dcd0f56cb8df71465d2236abcae57723fd37691e4dc80d09e0
```

The identity is compiled into the guests, so a stale ELF yields stale vkeys with no error
anywhere. Rebuild before reading them, and check that `elf/` is newer than
`tee-attestation/enclave-identity.toml`.

Read the vkeys back off any ISM and compare against the table in `deploy/DEPLOYMENT.md`:

```sh
cast call <TeeIsm> "stateTransitionVkey()(bytes32)" --rpc-url <rpc>
cast call <TeeIsm> "stateMembershipVkey()(bytes32)" --rpc-url <rpc>
celestia-appd query zkism ism <ism-id> --node https://rpc-mocha.pops.one -o json
```

## 4. The whole chain accepts a real quote

The three checks above compare values. This one runs the thing. It takes a live quote from an
enclave and executes both guests against it under SP1's mock prover, which enforces every
assertion - identity, RTMR replay, TCB status, report data, transition rules - in seconds
rather than the ninety minutes a real proof costs.

```sh
tee-hyperlane bootstrap-celestia --identity-digest <digest>
tee-hyperlane attest-celestia --enclave <cel-url> --trusted-state <genesis> \
  --merkle-tree-hook <hook-id> --out /tmp/att.json
SP1_PROVER=mock tee-hyperlane prove --attestation /tmp/att.json \
  --elf-dir tee-circuit/elf --out /tmp/proof.json
```

Worth running before creating ISMs: an identity the enclaves cannot satisfy produces vkeys
that are immutable once an ISM carries them, and the failure would otherwise surface an hour
into the first real proof.

If all four pass, then the code in this repo is the code in the enclave, and the enclave is
the only thing any ISM will accept a proof from.
