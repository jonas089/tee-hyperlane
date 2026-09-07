# Integrating a new chain

A chain joins the bridge as an **origin** (its state is attested) or a **destination** (it
stores an ISM). Most chains need both.

The two SP1 programs are origin-agnostic — they verify a TDX quote and project the attested
payload into the two shapes `x/zkism` reads. Adding a chain therefore adds **no circuits**.
What it adds is a way to reach a verified state root, and a way to read the Hyperlane merkle
tree under it.

Every origin implements the same two steps:

```rust
fn get_<chain>_root(store) -> AttestedRoot          // verified head + state root
fn get_<chain>_merkle_tree(root, proof) -> MerkleTree // the origin's Hyperlane tree
```

---

## Mainnet chains already covered

The testnet deployment maps one-to-one onto mainnet. Only endpoints, domains and the
weak-subjectivity checkpoint change.

**Ethereum** — sync-committee light client (`origins/ethereum.rs`). Bootstrap from a mainnet
finalized root, then MPT-prove the `MerkleTreeHook`'s 33 storage slots. Confirm the tree's
base storage slot against the live contract; it is deployment-specific (Hyperlane's Sepolia
hook uses 103, celestia-zkevm's testnet uses 151).

**Celestia** — Tendermint light client (`origins/celestia.rs`). Consensus only; no data
availability sampling, because Hyperlane messages live in the IAVL tree that the app hash
commits to. Note the off-by-one: `header[H].app_hash` commits to the state after block `H-1`.

**Arbitrum and Base** — no light client of their own. Both publish a commitment to their L2
state into Ethereum L1 storage, so once the Ethereum light client has a verified L1 root,
each L2 root costs a storage proof plus a keccak preimage check (`origins/ethereum_l2.rs`).
Mainnet differences: Arbitrum One is BoLD, so `latestConfirmed()` returns an assertion hash
rather than a node number and the preimage is an `AssertionState`, not
`keccak(blockHash ‖ sendRoot)`. Base mainnet uses the same output-root formula as Sepolia.

Both inherit their rollup's challenge window: only *confirmed* commitments are trustless, so
a message waits that window. On Base Sepolia the newest resolved dispute game trails the head
by roughly six days.

---

## A new EVM rollup

If it settles to a chain the bridge already attests, and publishes an L2 commitment into that
chain's storage, it needs one function and no light client.

1. Find where the L2 commitment lives in L1 storage. Two shapes cover most rollups:
   - **OP Stack** — an output root, `keccak(version ‖ stateRoot ‖ messagePasserStorageRoot ‖
     blockHash)`. The preimage contains the state root directly.
   - **Arbitrum Nitro** — `confirmData = keccak(blockHash ‖ sendRoot)`. `sendRoot` is the
     outbox root, *not* the state root, so you also supply the L2 block header RLP and take
     `header.stateRoot` (item 3).
2. Derive the slot from L1 rather than accepting it. Read `latestConfirmed` (or the anchor)
   out of storage and compute the mapping slot from it, so a relayer cannot point the enclave
   at a stale commitment. Watch for packed slots — Arbitrum Sepolia packs four `uint64`s into
   slot 117, and reading it whole gives a nonsense node number.
3. Add the arm to `OriginInput` and the config block. Everything downstream is unchanged.

Its Hyperlane tree is read exactly like Ethereum's, against the L2 state root you just
derived.

---

## Evolve-stack chains

An evolve chain posts its blocks to Celestia, so its state root is reachable from a Celestia
app hash rather than from its own consensus:

1. Verify Celestia as usual to get an app hash.
2. Read the namespace's blobs and check the sequencer's signature over them.
3. Re-execute, or take the state root the sequencer committed to, depending on how much you
   want the enclave to do.
4. Read the Hyperlane tree under that root — EVM-style if it runs ev-reth.

The bridge does not attempt this yet. The step that needs deciding is (3): re-executing inside
the enclave makes the enclave a full node for that chain, which is a materially bigger trusted
computing base than verifying consensus.

---

## An entirely new chain (Solana, Move, Cosmos)

The bridge assumes only three things. Meet them and the rest follows.

1. **A light client that fits in an enclave.** Pure verification, no I/O, small enough that
   the state fits in the ISM's `state` field (32..=2048 bytes). The enclave is stateless: the
   destination chain holds the light-client store and the enclave is handed it each time. If
   the store is large, commit to it with a hash — see `lc_store_commit`.
2. **A membership proof from the state root to Hyperlane's merkle tree.** MPT for EVM, ics23
   for Cosmos, and for Solana an account proof against the bank hash.
3. **A Hyperlane deployment whose merkle tree is the same incremental structure.** Solidity,
   hyperlane-cosmos and cw-hyperlane all agree here. Verify it: reproduce a live root from
   raw state before trusting anything.

Where implementations diverge is *unused* branch levels. hyperlane-cosmos pre-fills them with
the canonical zero hashes; Solidity leaves them zero. Compare `(count, root)`, never the raw
branch array.

**Destination side.** A new destination needs a contract or module that mirrors `x/zkism`:
store an opaque state whose first 32 bytes are the root, verify an SP1 Groth16 proof against
two pinned vkeys, authorise a batch of message ids per root, and consume each id once.
`contracts/src/TeeIsm.sol` is that port and is the shortest description of the protocol.

Non-EVM destinations need an SP1 Groth16 verifier on that chain. Celestia's is gnark in Go;
Solana would need a BN254 pairing, which its `alt_bn128` syscalls provide.

---

## Checklist

- [ ] `get_<chain>_root` verifies consensus, or derives the root from a chain that does
- [ ] `get_<chain>_merkle_tree` proves the tree against that root
- [ ] a live root reproduced from raw state, in a test
- [ ] domain id registered, and carried in the ISM state so it cannot be replayed cross-origin
- [ ] `Origin` variant, `OriginInput` arm, config block
- [ ] ISM deployed with the genesis state naming the checkpoint

---

## Operating notes that generalise

Two things learned wiring Celestia and Sepolia apply to any chain you add.

**Batches are whole tree ranges, not your messages.** The attested batch must contain every
leaf inserted between the ISM's trusted height and the attested head, because the merkle
replay reproduces the branch from all of them. On a shared mailbox that means other people's
message ids get authorised on your ISM. They are never delivered — their destination domain is
not yours — and the cost is a little state. If you deploy your own mailbox, the question does
not arise.

**Reaching back for the snapshot needs archive access.** The relayer proves the origin tree
twice: at the attested head, and at the ISM's trusted height to get the tree to replay onto.
The second is often outside a public node's proof window. Cache the tree you proved last
round; only bootstrap needs to reach back.
