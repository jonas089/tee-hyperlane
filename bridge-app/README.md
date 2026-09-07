# bridge-app

Bridges TIA and USDC between Celestia and the EVM testnets. MetaMask signs the EVM side,
Keplr the Celestia side.

```sh
npm install
npm run dev            # http://localhost:5173
npm run build          # -> dist/, served by nginx in production
```

`VITE_RELAYER_API` points at the attestation API (`tee-hyperlane serve`). It defaults to
`/api`, which is what nginx proxies in the server deployment, so no CORS is involved.

## What it shows

Each transfer has four steps, and the UI shows only those:

| step | means |
|---|---|
| Dispatched | the origin mailbox accepted it and put its id in the merkle tree |
| Attested by enclave | a TDX enclave verified the origin state containing it |
| Authorised by ISM | the destination ISM accepted both proofs and allowed this id |
| Delivered | the destination mailbox processed it |

Delivery is not inferred from the relayer. **Check** queries the destination mailbox
directly, so the UI's answer is the chain's answer.

Expanding a transfer shows the attestation for the batch it landed in: the origin block, the
attested state root, the enclave's image and OS measurements, and the TDX quote itself. That
is the point of the bridge and it is the only thing the UI goes out of its way to display.

## Configuration

`src/config.ts` holds every address. Adding a chain or a token is an entry there; the UI
hides routes whose warp routers are not deployed rather than offering something that will
fail.
