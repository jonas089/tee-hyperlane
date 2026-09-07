# End-to-end results

Four transfers, both directions, both tokens, on live testnets. No Hyperlane validators and
no Hyperlane relayer took part: every message was authorised by a light client running inside
a TDX enclave, proved on local CPU, and verified on chain.

## 1. TIA Mocha to Sepolia

```
Celestia header 569515 -> tendermint light client (in TDX)
  app hash -> ics23 x2 -> merkle tree, 3 leaves replayed
  TDX quote 5010 bytes -> 2 groth16 proofs, 278s + 269s, 260 bytes each
  TeeIsm.updateState / submitMessages / Mailbox.process
```

Synthetic TIA supply `0 -> 200000`. Message consumed once; a replay returns false.
Gas: updateState 273k, submitMessages 323k, process 127k/93k.

## 2 and 3. TIA and USDC Sepolia to Mocha

One batch carried both, plus three messages belonging to other users of Hyperlane's shared
Sepolia mailbox. The enclave replayed all six leaves — it must, or the branch cannot be
reproduced — and the ISM authorised all six. Only the three addressed to Celestia were
delivered; the rest were skipped on destination domain.

```
sync committee -> Sepolia finalized block 11656375
  MPT: account + 33 storage slots -> merkle tree, 6 leaves replayed
  2 groth16 proofs, 277s + 271s
  x/zkism update / submit-messages / mailbox process x3
```

```
utia                    698000 -> 748000     (+100000 delivered, -50000 fees)
hyperlane/...0002...0001      0 -> 1000000   (1 USDC minted by the ISM)
```

## 4. USDC Mocha to Sepolia

The first attestation to resume from a **non-empty** tree: one new leaf replayed onto the
three already there, which is what a running relayer does every tick rather than only at
bootstrap. It caught a real gap - `attest-celestia` had assumed an empty snapshot, which
would have worked exactly once.

Real USDC came back out of escrow, and the two sides agree:

```
my balance          19000000 -> 19400000   (+400000)
router collateral    1000000 ->   600000   (-400000)
```

Gas: updateState 272k, submitMessages 299k, process 108k.

## What this cost

Per batch, measured rather than estimated, on an M3 Max:

```
DCAP verification + event log replay   3.79M cycles
groth16 proof on CPU                   ~275 s each, two per batch
proof size                             260 bytes  (SP1 v5, what x/zkism accepts)
enclaves                               $2.92/day for both
```

A batch carries every message dispatched since the last one, so this is the cost per batch,
not per message. Latency is dominated by origin finality: Celestia finalises in a block,
Ethereum takes two epochs (~13 minutes) before a message can be attested at all.
