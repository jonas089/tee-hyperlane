# API

Relayer API (backs the `:3001` explorer).

```
B=http://<host>:3001          # direct
B=http://<host>:3000          # through the gateway: /api/... only, not / or /metrics
```

Schema: `$B/api/v1/openapi.json` (source: `tee-hyperlane/crates/tee-coprocessor/ui/openapi.json`). A test enforces it.

## Conventions

- Read-only, no auth.
- Times: unix seconds. Ids and 32-byte addresses: `0x` lowercase hex.
- v1 only gains fields; breaking changes are v2.

## Endpoints

| endpoint | returns |
|---|---|
| `GET /api/v1/health` | status and open problem counts; **HTTP 503 unless `ok`** |
| `GET /api/v1/inbox[?resolved=true]` | open notifications, most severe first; `resolved` adds last week's |
| `GET /api/v1/routes` | every route: what its loop is doing now (`activity`), its enclaves and how each last fared (`enclaves`), ISM height, origin head, its latest attestations with their transactions (`attestations`, heartbeats marked), when the next heartbeat is due, loop failures, staged batch, transfer counts, notifications |
| `GET /api/v1/routes/{name}` | one route |
| `GET /api/v1/chains` | every chain, and the tracker's watch on each origin |
| `GET /api/v1/wallets` | relayer gas wallet per destination: balance, daily spend (last week), days left, top-up flag |
| `GET /api/v1/messages[?status=&route=&limit=&offset=]` | transfers, problems first, then newest. `status` comma-separated. `limit` default 50, max 500 |
| `GET /api/v1/messages/{id}` | one transfer: timeline, verdict, failure, explorer links |
| `GET /api/v1/search?q=` | by message id, tx hash (origin or delivery), or account (sender or recipient; hex or `celestia1…`) |
| `GET /api/v1/upstream` | what `teeism upstream check` found: `checked_at`, `errors` (sources it could not read), `items` (upgrades and releases with `verdict` breaking, maybe, irrelevant or unjudged, `action`, `activates_at`, `acked_at`) |
| `GET /metrics` | Prometheus: `teeism_notifications_open{severity}`, `teeism_messages{route,status}`, `teeism_ism_height{route}`, `teeism_wallet_balance{chain}`, `teeism_wallet_days_left{chain}`, `teeism_last_sweep_seconds` |

## Transfer status

| status | meaning |
|---|---|
| `pending` | not yet covered by the ISM, within normal time. See `waitingOn` |
| `verified` | covered by the ISM; delivery next |
| `delivered` | processed by the destination mailbox |
| `failed` | destination refused; retried with backoff. See `failure` |
| `overdue` | stuck. See `problem` |

`overdue` when any of:

| condition | after |
|---|---|
| verified, not delivered | 10m |
| final on origin, not verified | 20m |
| older than route's `expectedLatencySecs` | Celestia 15m, Eden 45m, Sepolia 1h, Arbitrum 6h, Base 5d 6h |

## Notifications

Auto-open and auto-resolve; not dismissable. `severity`: `warning` or `critical`. Fields `subject`, `subjectId`.

| `key` prefix | when |
|---|---|
| `message-failed:<id>` | delivery refused |
| `message-overdue:<id>` | transfer stuck |
| `route-failing:<route>` | route loop failing 10m (critical after 1h) |
| `route-uncovered:<route>` | relayer cannot read the origin between the height it has accounted for and the ISM's for 10m (critical after 1h); messages there may be attested but undelivered |
| `route-silent:<route>` | route loop silent 45m |
| `route-staged:<route>` | attested batch submitting 30m |
| `route-enclaves:<route>` | a route with several enclaves has one failing (critical when all are) |
| `ism-unreadable:<route>` | ISM unreadable 10m |
| `watch-failing:<chain>` | tracker can't read origin 10m; transfers unwatched |
| `wallet-low:<chain>` | below warning level or under 7 days left (critical: empty or under 2 days) |
| `wallet-unreadable:<chain>` | balance unreadable 10m |
| `monitor-silent` | the monitor stopped |

## Gas wallets

- Read every 5 minutes. Spend = sum of drops over the last week (top-ups ignored); no estimate before 6 hours.
- Override the warning level with `low_balance` (whole tokens) per chain in the coprocessor config.

| chain | default warning level |
|---|---|
| Sepolia | 0.05 ETH |
| L2s | 0.01 ETH |
| Eden | 1 TIA |
| Celestia | 5 TIA |

## Examples

```sh
curl -s $B/api/v1/health | jq                                   # ok / degraded / down
curl -s $B/api/v1/inbox | jq '.notifications[] | {severity, title, detail}'
curl -s "$B/api/v1/messages?status=overdue,failed" | jq '.messages[] | {id, route, problem}'
curl -s "$B/api/v1/search?q=0x318d22faa1e0f29eac7ef644a8fac676f6688d1e" | jq '.messages[] | {id, status}'
curl -s $B/api/v1/routes | jq '.[] | {name, status, ism: .ismReading.height}'
curl -s $B/api/v1/wallets | jq '.[] | {chain, display, symbol, daysLeft, level}'
```

Alerting: probe `/api/v1/health` from outside the host. Non-200 or no answer is a problem.

## Older endpoints

For the bridge app only: `/api/health`, `/api/status`, `/api/attestation/{message_id}`, `/api/faucet`, `/api/faucet/{address}`. Use `/api/v1` for new tools.
