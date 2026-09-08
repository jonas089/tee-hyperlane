# Verifying what is running

Three things are worth checking independently, and none of them requires trusting this repo's
word for it.

## 1. The enclaves run the image the circuits pin

The image is built with Nix, so the digest is reproducible: build it yourself and compare.
A `docker build` digest could only ever be taken on trust, because it bakes in whatever the
builder's machine happened to have.

```sh
nix build .#image                          # ~10 min cold, from a clean checkout
docker load -i result                      # prints the image id
docker inspect --format '{{.Id}}' ghcr.io/jonas089/tee-node:reproducible
```

That id must equal the digest pinned in `deploy/docker-compose.yml`:

```sh
grep image: deploy/docker-compose.yml
```

Both are `sha256:0109b978faed93a63e984d6a1dc1b713bf236e14c6d7413886a04e603b1f2d2f`.

To confirm the build really is reproducible rather than merely repeatable on one machine:

```sh
nix build .#image --rebuild                # rebuilds and diffs; silent means identical
```

## 2. The running enclaves measure to the pinned identity

`compose-hash` lands in the enclave's RTMR3, and the circuits pin it. It is *not* the sha256
of `docker-compose.yml`: dstack hashes its own app-compose document, which wraps that file. So
the check is not "hash the file" but "ask a live enclave what it measured and compare against
what is pinned", which closes the same loop without having to reimplement their encoding.

```sh
cd tee-circuit
for app in d37cfd9c3598f2335cd3c6c4a1f76da46407d421 f8da571c89be34182a39099d2c0684f6d46dfd75; do
  cargo run -q -p circuit-tool -- identity \
    --url https://$app-8080.dstack-pha-prod9.phala.network \
    | grep -E 'mr_td|os_image_hash|compose_hash|mr_kms' > /tmp/live-$app
done
# The two enclaves must agree with each other, and with what the circuits pin.
diff /tmp/live-d37cfd9c3598f2335cd3c6c4a1f76da46407d421 /tmp/live-f8da571c89be34182a39099d2c0684f6d46dfd75
diff /tmp/live-d37cfd9c3598f2335cd3c6c4a1f76da46407d421 \
     <(grep -E 'mr_td|os_image_hash|compose_hash|mr_kms' tee-attestation/enclave-identity.toml)
```

Both diffs are empty today. The values are:

```
mr_td         f06dfda6dce1cf904d4e2bab1dc370634cf95cefa2ceb2de2eee127c93826980…
os_image_hash bd369a8c2f9edb2b52dad48ac8e0b32dde5f1337c423a506b48d07403a7d8033
compose_hash  bb35e830a2ec224642e9ae963e964d5941e1d5ae1a1256eca4c9550bb2c42b6d
mr_kms        92a4bf40c88734b0e56f54b09b1f0fe4b8d3e230047e9298f491968ada8dedf8
```

`app-id` and `instance-id` are deliberately *not* pinned. They differ per CVM, while
`compose-hash` already fixes the code, which is what lets one identity cover both enclaves and
lets an enclave be replaced without touching an ISM.

## 3. The ISMs pin the vkeys those circuits produce

```sh
cd tee-circuit && cargo run -p circuit-tool -- vkeys
```

```
tee-state-transition  0x00350b158e64c20dc65eb4eaa2445da8e68264a6ebc092cd9104d9464d1ce458
tee-state-membership  0x0028f85f3f0a3d431b8b1b7804d1ca0c2024dbb70cb303e043724b63bb257c99
identity digest       5d6083a9631b6f75d20489febe15f619bd2d0953f6d9f574713230cf275d6c5a
```

Read them back off any ISM and compare:

```sh
cast call <TeeIsm> "stateTransitionVkey()(bytes32)" --rpc-url <rpc>
cast call <TeeIsm> "stateMembershipVkey()(bytes32)" --rpc-url <rpc>
celestia-appd query zkism ism <ism-id> --node https://rpc-mocha.pops.one -o json
```

If all three checks pass, then the code in this repo is the code in the enclave, and the
enclave is the only thing any ISM will accept a proof from.
