# teeism-mcp

An MCP server for the TEE Interchain Solutions venue: quote, trade, launch tokens and add
liquidity across Celestia, Sepolia, Base and Arbitrum, with every transfer verified by TEE ISMs.

```sh
npm install
claude mcp add teeism -e TEEISM_URL=http://<gateway>:3000 \
  -e EVM_PRIVATE_KEY=0x… -e CELESTIA_MNEMONIC="…" -- node $PWD/server.mjs
```

Without keys it still quotes, reads balances and returns unsigned transactions (`build_step`).
The tools and the HTTP API under them are documented in the bridge app's Trade tab, under
Docs (Agents), and in `deploy/API.md`.

`hub.mjs` encodes the Celestia messages the API returns, with the same field numbers as
`bridge-app/src/celestia.ts`; keep the two in step.
