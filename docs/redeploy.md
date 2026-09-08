# Replacing an enclave

An enclave is disposable. It holds no keys, no state and no disk - the destination chain's
ISM is the light client's only database, so a new CVM picks up exactly where the old one left
off. Replacing one is: deploy, re-pin, point the relayer at the new URL.

What must **not** change is the enclave's *identity*: the OS image, the container image and
the KMS. Those are compiled into the circuits, so an enclave that measures differently
produces proofs no ISM will accept. The app id and instance id are deliberately not pinned,
which is what makes this a swap rather than a migration.

## The swap

```sh
# 1. Deploy a CVM on the same compose file and the same non-dev OS image.
phala deploy --node-id <teepod> --image dstack-0.5.9 -c deploy/docker-compose.yml

# 2. Check it measures the same as the ISMs already trust.
curl https://<app-id>-8080.<gateway>/identity | jq -r .identity_digest
# must equal the digest in README's Deployments section
```

If the digest matches, there is nothing else to do: edit `tee_node_url` for that route in
`coprocessor.toml` and restart the relayer.

```sh
systemctl restart tee-hyperlane
curl -s localhost:3001/api/status | jq '.[] | {name, height}'
```

The heights should be the same ones the old enclave left behind, and the next tick advances
them. Nothing needs to be replayed, because the relayer derives its whole starting position
from the ISM.

## When the digest does not match

Then the image or the OS changed, and the old vkeys no longer describe this enclave. That is
a new identity, and the ISMs that trust the old one cannot be updated to trust it - an ISM's
vkeys are immutable by design. The path is to re-pin and deploy fresh ISMs:

```sh
curl https://<app-id>-8080.<gateway>/identity > identity.json
cd tee-circuit
cargo run -p circuit-tool -- identity --url https://<app-id>-8080.<gateway> --write
cargo run -p circuit-tool -- build
cargo run -p circuit-tool -- vkeys        # new identity digest + two vkeys
```

Then deploy new ISMs with those vkeys (`deploy/deploy-l2-ism.sh` for an EVM chain,
`MsgCreateInterchainSecurityModule` for Celestia), point the warp routers at them, and update
`coprocessor.toml`. The old ISMs keep working for anything already in flight.

## Why nothing goes stale

Three properties, each worth checking if a redeploy ever seems to hang:

**The relayer has no memory.** Every tick starts from `read_ism_state`, and even the Ethereum
light-client checkpoint is derived from the ISM's own trusted timestamp rather than stored. A
relayer that has been off for a week resumes from what the chain says.

**An interrupted batch is finished, not abandoned.** A proved batch is written to
`proofs/<route>/staging/proved.json` before submission and only filed away once the
destination has it. On restart the relayer submits that batch before doing anything new,
because advancing past it would strand its messages permanently.

**Historical reads need an archive node.** Resuming from a trusted height older than about
128 blocks needs state proofs a public node has already pruned. Set `archive_rpc` on the
origin - without it a long outage ends with the route stuck rather than merely behind:

```toml
[routes.origin]
kind = "ethereum"
execution_rpc = "https://..."
archive_rpc = "https://..."   # serves eth_getProof at the trusted height
```

## Moving off Phala

Nothing in the enclave is Phala-specific except the dstack socket it asks for a quote. Any
TDX host that runs the same compose file and reports the same measurements satisfies the same
circuits, and the ISMs cannot tell the difference - they were never told which instance, or
which provider, to expect.
