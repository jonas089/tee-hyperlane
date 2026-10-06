# API

The relayer's API: every transfer on our routes, every route, and every open problem, which the
explorer page on `:3001` is built on; and the venue under `/api/v1/trade`, which the Trade tab
and the MCP server in `mcp/` are built on.

```
B=http://<host>:3001          # direct
B=http://<host>:3000          # through the gateway: /api/... only, not / or /metrics
```

The full schema is `$B/api/v1/openapi.json` (source: `tee-hyperlane/crates/tee-coprocessor/ui/openapi.json`).
A test fails if a served field and the spec disagree.

## Conventions

- Read-only, no authentication: everything is public chain data or a report about it.
- Times are unix seconds. Message ids and 32-byte addresses are `0x`-prefixed lowercase hex.
- v1 only ever gains fields. A removed field or a changed meaning is v2.

## Endpoints

| endpoint | returns |
|---|---|
| `GET /api/v1/health` | overall status and open problem counts; **HTTP 503 unless `ok`** |
| `GET /api/v1/inbox[?resolved=true]` | open notifications, most severe first; with `resolved`, the last week's too |
| `GET /api/v1/routes` | every route: ISM height, origin head, loop failures, staged batch, transfer counts, its notifications |
| `GET /api/v1/routes/{name}` | one route |
| `GET /api/v1/chains` | every chain, and the tracker's watch on each origin |
| `GET /api/v1/wallets` | the relayer's gas wallet on each destination: balance, spend per day over the last week, days left, and whether to top up |
| `GET /api/v1/messages[?status=&route=&limit=&offset=]` | transfers, problems first, then newest. `status` is comma-separated. `limit` defaults to 50, max 500 |
| `GET /api/v1/messages/{id}` | one transfer, with its timeline, verdict, failure and explorer links |
| `GET /api/v1/search?q=` | transfers matching a message id, a transaction hash (origin or delivery), or an account (sender or recipient; hex or `celestia1…`) |
| `GET /metrics` | Prometheus: `teeism_notifications_open{severity}`, `teeism_messages{route,status}`, `teeism_ism_height{route}`, `teeism_wallet_balance{chain}`, `teeism_wallet_days_left{chain}`, `teeism_last_sweep_seconds` |

## Transfer status

| status | meaning |
|---|---|
| `pending` | dispatched, not yet covered by the ISM, within the route's normal time. `waitingOn` says what for |
| `verified` | covered by the ISM; delivery is next |
| `delivered` | processed by the destination mailbox |
| `failed` | the destination refused delivery; parked and retried with backoff. `failure` has the reason |
| `overdue` | stuck. `problem` says where |

A transfer is `overdue` when it is verified but not delivered after 10 minutes, final on its
origin but not verified after 20 minutes, or older than its route's `expectedLatencySecs`
(Celestia 15m, Eden 45m, Sepolia 1h, Arbitrum 6h, Base 5d 6h).

## Notifications

A notification opens when its condition first holds, resolves by itself when it clears, and
cannot be dismissed. `severity` is `warning` or `critical`; `subject` and `subjectId` say what it
is about.

| `key` prefix | when |
|---|---|
| `message-failed:<id>` | a delivery was refused |
| `message-overdue:<id>` | a transfer is stuck |
| `route-failing:<route>` | the route loop has been failing for 10m (critical after 1h) |
| `route-silent:<route>` | the route loop has reported nothing for 45m |
| `route-staged:<route>` | an attested batch has been submitting for 30m |
| `ism-unreadable:<route>` | the ISM could not be read for 10m |
| `watch-failing:<chain>` | the tracker could not read an origin for 10m: its transfers are unwatched |
| `wallet-low:<chain>` | the relayer's gas wallet is below its warning level or under 7 days from empty (critical: empty, or under 2 days) |
| `wallet-unreadable:<chain>` | the wallet's balance could not be read for 10m |
| `monitor-silent` | the monitor itself has stopped |

## Gas wallets

Read every 5 minutes. Spend is the sum of drops between readings over the last week, so a
top-up does not count, and there is no estimate before 6 hours of readings. The warning level
defaults to 0.05 ETH on Sepolia, 0.01 ETH on the L2s, 1 TIA on Eden and 5 TIA on Celestia; set
`low_balance` (whole tokens) on a chain in the coprocessor config to change it.

## Examples

```sh
curl -s $B/api/v1/health | jq                                   # ok / degraded / down
curl -s $B/api/v1/inbox | jq '.notifications[] | {severity, title, detail}'
curl -s "$B/api/v1/messages?status=overdue,failed" | jq '.messages[] | {id, route, problem}'
curl -s "$B/api/v1/search?q=0x318d22faa1e0f29eac7ef644a8fac676f6688d1e" | jq '.messages[] | {id, status}'
curl -s $B/api/v1/routes | jq '.[] | {name, status, ism: .ismReading.height}'
curl -s $B/api/v1/wallets | jq '.[] | {chain, display, symbol, daysLeft, level}'
```

For alerting, probe `/api/v1/health` from outside the host: any non-200 is a problem, and no
answer means the relayer or the host is down.

## The venue: `/api/v1/trade`

On when the config has a `[trade]` table, which `write_config` writes once teeUSD exists. Nothing
here signs: every endpoint returns unsigned transactions, `evm` ones to send as they are and
`cosmos` ones whose `msgs` go in one Celestia transaction, in order. Amounts are base units as
strings, every asset 6 decimals. An asset is named by its `id`: the symbol for TIA and teeUSD,
the hub token id for a launched token.

| | |
|---|---|
| `GET /api/v1/trade` | chains, pools and every listed asset; `?refresh=true` re-reads now |
| `GET /api/v1/trade/quote?from&sell&to&buy&amount` | the price and the steps |
| `POST /api/v1/trade/build` | one step to transactions, for the amount actually held |
| `POST /api/v1/trade/pool` | full-range liquidity, creating the pool if needed |
| `POST /api/v1/trade/launch/create`, `/deploy`, `/wire` | the three steps of a launch |
| `GET /api/v1/trade/launch/{id}` | a launch's routers, and its listing once wired |

A swap takes the direct pool for a pair or the route through teeUSD, whichever pays more. A
launched token is listed once the hub shows it with no owner, on the routing ISM, and with every
remote router one a `TeeTokenFactory` made; its routers then join `.state/proofs/launched.json`,
which every route and the tracker accept beside the configured ones. Launches and pools are
re-read every 30 seconds.

```sh
curl -s "$B/api/v1/trade/quote?from=celestia&sell=TIA&to=base&buy=teeUSD&amount=10000000" | jq
```

## Older endpoints

Kept for the bridge app: `/api/health`, `/api/status`, `/api/attestation/{message_id}`,
`/api/faucet`, `/api/faucet/{address}`. New tools should use `/api/v1`.
