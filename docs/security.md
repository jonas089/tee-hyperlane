# What the bridge trusts, and why each check exists

Every check in the enclave and in `TeeIsm.sol` is here because leaving it out lets someone
move or destroy a message. This is the long form; the code carries one-line pointers back to
it rather than the argument, so the argument stays in one place and does not drift between
five files.

## The trust boundary

**Trusted.** Intel TDX and the DCAP PKI, whose root CA is compiled into the guest ELFs. The
pinned measurement policy, which the circuit vkeys commit to. The ISM's genesis state, which
pins the light-client checkpoint and is public at ISM creation. The two SP1 vkeys and the
gnark Groth16 wrap key.

**Not trusted.** Every RPC - beacon, execution, Celestia, PCCS - which are data sources only.
The coprocessor and relayer, which can stall but never forge. Phala as operator, beyond
liveness. The host clock. Every field of an `AttestRequest`.

That last one is the recurring source of bugs. A request is attacker-controlled in full: the
enclave endpoint is public, and anyone can post to it. Four of the fixes below are the same
mistake in different clothes - a value that looked like configuration was in fact a request
field, and proving something *about* it proved nothing about the bridge.

## Why the enclave verifies inclusion, rather than the chain

The ISM's root comes from the enclave. A compromised enclave can attest a fabricated root,
and from a fabricated root it can produce a valid inclusion proof for any message. An
on-chain inclusion proof under a TEE-attested root is therefore redundant: it would only add
security if the root came from a stronger source, which it does not. What it would add is an
MPT and ics23 verifier inside the SP1 guests, at unmeasured cycle cost.

The residual difference is worth stating plainly: a TDX break forges *messages* directly
rather than forging a root and then a proof under it. Same adversary, same break, one fewer
step.

## The checks, and what each one stops

### The merkle tree address must be the one being attested

Anyone can deploy a merkle tree hook, fill it with message ids of their choosing, and prove
it honestly against the real origin state root. The proof is valid; it is simply about the
wrong tree. `build_attested_update` requires the address a tree was proven at to equal the
address the ISM pins, for both the head tree and the snapshot.

### The L2 anchor contract is pinned, not named by the caller

Same shape, on L1. Anyone can deploy a contract whose storage mimics Arbitrum's `RollupCore`
or Base's `AnchorStateRegistry`, put any L2 state root in it, and prove that storage
perfectly against the real L1 state root. `L2Anchor` therefore holds the addresses and the
slot layout as constants, which puts them under `compose_hash` - so changing one is a
redeploy of the whole identity, not a field in a JSON body.

### The snapshot is read under the ISM's own state root

The enclave replays a snapshot plus the claimed message ids and requires the result to equal
the tree it proved at the new head. That check pins the *span*, not where the span starts.

A Hyperlane tree is incremental and every leaf that ever entered it is public, so anyone can
rebuild the exact tree the origin held at any past count. Hand the enclave the head minus one
leaf, plus the single id that closes the gap, and the replay reproduces the head's count and
root exactly. Everything in between is skipped - and skipped for good, because the root has
moved on, that root's one submission slot is spent, and a later batch starts from the new
head. It is worse than censoring everything, because it is aimed: drop one transfer, let the
rest through, and the bridge looks healthy.

The fix is not a better span check. Both ends are now read from state the ISM already
trusts - the snapshot proven under `prev_state.state_root`, the head under the root being
attested - so the replay covers exactly the distance the ISM is moving. An empty batch stops
being an attack as a side effect; it is still refused, because a batch that authorises
nothing spends that state root's only submission slot.

### The clock is bound to attested chain time, not to the state's own timestamp

The host clock is not trusted, so freshness is bounded against something the enclave verified
this round. For an L2 origin that anchor is the *L1* head, not the L2 root's timestamp: an
optimistic rollup's confirmed head is old on purpose, and that lag is the fraud-proof window,
not evidence of staleness. Bounding against the L2 timestamp rejected every honest L2 proof,
which is why neither L2 route produced a batch until it was corrected.

The anchor may not predate the state it carries, or a fresh L1 header could be paired with an
arbitrarily old L2 root - the rollup lag is exactly the cover that would hide it.

### `TeeIsm.verify` is Mailbox-only

`verify` is not a query: it deletes the authorisation it finds. Left open, anyone could take
a message id out of the origin's `Dispatch` log, call `verify` directly, and burn the
authorisation before the relayer delivers it. Re-authorising is impossible - the batch is
already submitted for this root, the next snapshot already contains those leaves, and the
state may not move backwards - so the transfer's tokens would be stranded permanently.

### The state root must change on every update

`x/zkism` resets its one-batch-per-root flag only when `state[:32]` changes. Without an
in-circuit requirement that the root advances, the relayer silently wedges after one batch.

## Residual risks

- A TDX break, or an unrevoked but vulnerable TCB, forges any root. This is the irreducible
  assumption. The TCB-status allowlist is the only lever, and it trades liveness for safety
  on TCB-recovery days.
- The enclave operator can withhold attestations. Funds are never at risk, but a bridge that
  does not advance is a bridge that is down.
- `x/zkism` does no freshness check of its own. The EVM side enforces `maxStateAge`; Celestia
  cannot reject a stale but well-formed state.
- Light-client security is the standard model: a fork needs a third of the *trusted* validator
  set or sync committee to equivocate.
- L2 roots are trustless only once confirmed, which is gated on each chain's challenge window.
- Owner keys for the ISMs and warp routers are single EOAs today. They should be a multisig.
