# INTERACT

Using a running bridge: wallets, test funds, sending, and checking arrival yourself. `B` is
the gateway, e.g. `B=http://<host>:3000`; `celestia-appd` commands run on the host.

---

## Endpoints

```
:3000  the UI, and the only public way to reach the chain
       /rpc  /rest            Celestia RPC and REST
       /api                   relayer API
       /evm/<chain>/          EVM JSON-RPC for sepolia, arbitrum, base, eden
       /tx/<hash>             transaction view
:3001  relayer dashboard       :3002  gas oracle dashboard
```

## Wallets

- **Keplr.** Open the UI and connect; it registers `teeism-local` for you. Import
  `CELESTIA_MNEMONIC` from `devnet/.env`. Account 0 is funded, and stays funded across chain
  rebuilds.
- **MetaMask.** Let the UI add the networks. It points them at `$B/evm/<chain>/`.

## Test funds

The **Faucet** tab gives 1000 TIA, once per address. From the command line:

```sh
curl -s $B/api/faucet                                   # {"enabled":true,"amountTia":1000}
curl -s -X POST $B/api/faucet -H 'content-type: application/json' \
  -d '{"address":"celestia1..."}'                       # {"tx_hash":"...","amount_tia":1000}
```

A second claim returns 409. Claims are recorded on the host, so a new browser does not reset
them.

## Sending

**In the UI:** pick origin, destination, asset and amount. USDC from Sepolia needs an
`approve` first, which the UI sends. A synthetic is burned, so it needs none.

**From Celestia:**

```sh
celestia-appd tx warp transfer <celestia token id> <destination domain> \
  0x000000000000000000000000<evm address without 0x> <amount> \
  --from <key> --chain-id teeism-local --node tcp://localhost:26657 \
  --gas auto --gas-adjustment 1.5 --gas-prices 0.002utia --max-hyperlane-fee 10000000utia -y
```

`--gas auto` prints `gas estimate: <n>` before the JSON, so `-o json | jq` fails, and by then
the transfer has already been broadcast. Read the `txhash` line instead of re-running.

**From an EVM chain** (a collateral router needs an `approve` first):

```sh
cast send <router> "transferRemote(uint32,bytes32,uint256)(bytes32)" \
  1297040299 0x000000000000000000000000<celestia address as 20 hex bytes> <amount> \
  --value $(cast call <router> "quoteGasPayment(uint32)(uint256)" 1297040299 --rpc-url <rpc>) \
  --rpc-url <rpc> --private-key $EVM_PRIVATE_KEY
```

Domains: Celestia `1297040299`, Sepolia `11155111`, Arbitrum `421614`, Base `84532`, Eden
`3735928814`. Router and token addresses are in [README.md](../README.md#deployments).

## How long it takes

```
Celestia -> Arbitrum, Base, Sepolia   under 30 s
Celestia -> Eden                      ~30 s
Eden     -> Celestia                  1-4 min     waiting for Eden to post its headers to mocha
Sepolia  -> Celestia                  ~15 min     Ethereum finality
Arbitrum -> Celestia                  ~1h 40m     how far Arbitrum's confirmed root trails
Base     -> Celestia                  ~5 days     Base's dispute window
```

The slow ones are the origin's own finality; the bridge adds one transaction. A Base transfer
sitting for days is normal ([MAINTAIN](MAINTAIN.md#is-it-waiting-or-stuck)).

## Did it arrive

The destination mailbox is the authority:

```sh
cast call <mailbox> "delivered(bytes32)(bool)" <message id> --rpc-url <rpc>
celestia-appd query hyperlane delivered <mailbox id> <message id> --node tcp://localhost:26657
```

Ask Celestia only about messages *into* Celestia: it records its own outgoing ids in the same
set, so an outbound id reads `true` as soon as it is sent.

Or read the balance. On an EVM chain the router is the ERC20
(`cast call <router> "balanceOf(address)(uint256)" <you> --rpc-url <rpc>`). On Celestia a
synthetic is the bank denom `hyperlane/<token id>`.

## Watching it

```sh
curl -s $B/api/status | jq '.[] | {name, height, timestamp, proving, blocked}'
journalctl -u teeism-relayer -f | grep -E 'attesting|delivering|delivered|route failed'
```

A healthy cycle:

```
route: attesting route=base-to-celestia from=46889663 to=46890863 leaves=2
destination: delivering id=...
route: batch delivered route=base-to-celestia height=46890863
```

The UI shows four steps: **Dispatched**, **Attested by enclave**, **Authorised by ISM**,
**Delivered**. Delivered comes from the destination mailbox, not from the relayer. Expanding a
transfer shows its batch's attestation: the origin block, the state root, the enclave
measurements and the quote.

## What an ISM trusts

```sh
cast call <TeeDcapIsm> "state()(bytes)" --rpc-url <rpc>
celestia-appd query teeism ism <ism id> --node tcp://localhost:26657 -o json
```

```
[  0: 32] state_root     [ 32: 36] origin_domain   [ 36: 44] height
[ 44: 52] timestamp      [ 52: 84] lc_store_commit [ 84:116] identity_digest
```

`height` and `timestamp` show how far the ISM has followed its origin.

## Cost

One attestation covers a whole batch. At 1.1 gwei on Sepolia, 0.006 on Base, 0.25 on Arbitrum:

```
                      gas         sepolia    base       arbitrum    (ETH)
attestation           5,331,900   0.005900   0.000031   0.001330
delivery, per message   124,000   0.000140   0.000001   0.000030
```

On Celestia an attestation is 343,592 gas and a delivery 117,337. The enclaves cost
$4.38/day for all three. Senders pay through the paymaster; the gas oracle at `:3002` shows the
current quotes, and `~/tee-ism-nonzk/tee-hyperlane/target/release/gas-oracle --config ~/tee-ism-nonzk/devnet/.state/gas-oracle.toml --once` prints one round.

## Running the UI locally

```sh
cd bridge-app && npm install && npm run dev     # http://localhost:5173
```

It reads every address from `VITE_*` in `.env.local` ([DEPLOY step 11](DEPLOY.md#11-ui-and-gateway)).
`make start` (in the repo root) writes one pointing at a local devnet.
