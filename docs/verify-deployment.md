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

Both are `sha256:55dfce65f16be7c0cb95f858f7443bfa8b3634b1227d95f569022901d59d8808`.

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
for app in 596ed37171fa16da4a9ba5afaec8f19cb8b2860d a3feb765232e08e567d5f7de773db9dac55e7d5b; do
  cargo run -q -p circuit-tool -- identity \
    --url https://$app-8080.dstack-pha-prod9.phala.network \
    | grep -E 'mr_td|os_image_hash|compose_hash|mr_kms' > /tmp/live-$app
done
# The two enclaves must agree with each other, and with what the circuits pin.
diff /tmp/live-596ed37171fa16da4a9ba5afaec8f19cb8b2860d /tmp/live-a3feb765232e08e567d5f7de773db9dac55e7d5b
diff /tmp/live-596ed37171fa16da4a9ba5afaec8f19cb8b2860d \
     <(grep -E 'mr_td|os_image_hash|compose_hash|mr_kms' tee-attestation/enclave-identity.toml)
```

Both diffs are empty today. The values are:

```
mr_td         f06dfda6dce1cf904d4e2bab1dc370634cf95cefa2ceb2de2eee127c93826980…
os_image_hash bd369a8c2f9edb2b52dad48ac8e0b32dde5f1337c423a506b48d07403a7d8033
compose_hash  99e157b98b57729bbd6b97adc76964897356b40406ca7b0942214dc1511e64e8
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
tee-state-transition  0x000d223dcbccdb71106e4205f81caa8cfeefab16157898b120ca5908f47e53f7
tee-state-membership  0x00cb0f3e39c2f501de946600002a68409db9aab53bf1f31284231225298829df
identity digest       241dd0baf8e3065e22732b08ed99c286d6ec10958ca2885c26a381324dca0a95
```

Read them back off any ISM and compare:

```sh
cast call <TeeIsm> "stateTransitionVkey()(bytes32)" --rpc-url <rpc>
cast call <TeeIsm> "stateMembershipVkey()(bytes32)" --rpc-url <rpc>
celestia-appd query zkism ism <ism-id> --node https://rpc-mocha.pops.one -o json
```

If all three checks pass, then the code in this repo is the code in the enclave, and the
enclave is the only thing any ISM will accept a proof from.
