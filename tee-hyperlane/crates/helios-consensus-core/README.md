# helios-consensus-core

a16z/helios's `ethereum/consensus-core` at rev `43a8c9f3cdda41a6f383c4db41d9a83f102638b1`,
vendored to add Gloas, which Sepolia activated on 2026-10-06 at slot 11296768 and upstream does
not support. Every change from upstream is marked `Gloas:`.

In Gloas a light-client header carries the execution block hash, not an execution payload
header, so the enclave takes the execution header from the relayer and checks its hash. Proof
positions are Lodestar v1.49.0's (`packages/params/src/index.ts`).
