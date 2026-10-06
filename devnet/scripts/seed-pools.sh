#!/usr/bin/env bash
# One-off: seed a TIA/teeUSD Uniswap v3 pool on Sepolia, Base and Arbitrum. Delete after use.
#
# Both sides are bridged from Celestia to the EVM deployer, which then owns each position:
# TIA from the `validator` key, teeUSD from the `user` key that 50-warp-celestia.sh minted to.
# Every step checks the chain first and records each bridge send, so re-running it waits for
# transfers in flight rather than sending them twice.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need cast
: "${EVM_PRIVATE_KEY:?set EVM_PRIVATE_KEY}"

TIA_AMOUNT="${TIA_AMOUNT:-1000000000000}"      # 1M TIA per pool
TEEUSD_AMOUNT="${TEEUSD_AMOUNT:-500000000000}" # 500k teeUSD per pool, so 1 TIA = 0.5 teeUSD
FEE=3000
TICK=887220                                     # full range at tick spacing 60

# chain : domain : factory : position manager
VENUES="sepolia:${SEPOLIA_DOMAIN}:0x0227628f3F023bb0B980b67D528571c95c6DaC1c:0x1238536071E1c677A632429e3655c799b22cDA52
base:${BASE_SEPOLIA_DOMAIN}:0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24:0x27F971cb582BF9E50F397e4d29a5C7A34f11faA2
arbitrum:${ARBITRUM_SEPOLIA_DOMAIN}:0x248AB79Bbb9bC29bB72f7Cd42F17e054Fc40188e:0x6b2937Bde17889EDCf8fbD8dE31C3C2a70Bc4d65"

ZERO=0x0000000000000000000000000000000000000000
ME="$(cast wallet address --private-key "${EVM_PRIVATE_KEY}")"
lower() { printf '%s' "$1" | tr 'A-F' 'a-f'; }
rpc_for() { python3 -c "import json;print(json.load(open('${OUT_DIR}/pccs-$1.json'))['rpc'])"; }
balance() { cast call "$1" "balanceOf(address)(uint256)" "${ME}" --rpc-url "$2" | cut -d' ' -f1; }
send() { cast send "$@" --private-key "${EVM_PRIVATE_KEY}" >/dev/null; sleep 4; }

wait_for_chain
say "deployer ${ME}"

# 1. Bridge what each pool needs, unless it is already there or already on its way.
fund() { # <chain> <domain> <rpc> <label> <evm token> <celestia token> <key> <amount>
  local chain="$1" domain="$2" rpc="$3" label="$4" evm="$5" cel="$6" key="$7" amount="$8" res
  if has "pool-fund-${chain}-${label}"; then
    say "${chain}: ${label} already sent in $(load "pool-fund-${chain}-${label}")"
    return 0
  fi
  [ "$(balance "${evm}" "${rpc}")" -ge "${amount}" ] && return 0
  say "${chain}: bridging ${amount} ${label} from ${key}"
  res="$(tx "${key}" warp transfer "${cel}" "${domain}" "$(pad32 "${ME}")" "${amount}" \
    --max-hyperlane-fee 10000000utia)"
  save "pool-fund-${chain}-${label}" "$(printf '%s' "${res}" | jq -r .txhash)"
}
while IFS=: read -r chain domain factory npm; do
  rpc="$(rpc_for "${chain}")"
  fund "${chain}" "${domain}" "${rpc}" tia "$(load "${chain}-router")" "$(load celestia-token-id)" validator "${TIA_AMOUNT}"
  fund "${chain}" "${domain}" "${rpc}" teeusd "$(load "${chain}-teeusd-router")" "$(load celestia-teeusd-token-id)" user "${TEEUSD_AMOUNT}"
done <<< "${VENUES}"

# 2. Per chain, once both sides have landed: create the pool at the price and add full range.
while IFS=: read -r chain domain factory npm; do
  rpc="$(rpc_for "${chain}")"
  tia="$(load "${chain}-router")"
  usd="$(load "${chain}-teeusd-router")"

  say "${chain}: waiting for both sides to arrive"
  for i in $(seq 1 180); do
    [ "$(balance "${tia}" "${rpc}")" -ge "${TIA_AMOUNT}" ] && [ "$(balance "${usd}" "${rpc}")" -ge "${TEEUSD_AMOUNT}" ] && break
    [ "${i}" = 180 ] && die "${chain}: funds not there after 30 minutes; check the relayer, then re-run"
    sleep 10
  done

  # Uniswap orders a pair by address, and the price is token1 per token0.
  if [[ "$(lower "${tia}")" < "$(lower "${usd}")" ]]; then
    t0="${tia}" t1="${usd}" a0="${TIA_AMOUNT}" a1="${TEEUSD_AMOUNT}"
  else
    t0="${usd}" t1="${tia}" a0="${TEEUSD_AMOUNT}" a1="${TIA_AMOUNT}"
  fi
  sqrt="$(python3 -c "import math;print(math.isqrt(${a1} * 2**192 // ${a0}))")"

  pool="$(cast call "${factory}" "getPool(address,address,uint24)(address)" "${t0}" "${t1}" "${FEE}" --rpc-url "${rpc}")"
  if [ "${pool}" = "${ZERO}" ]; then
    say "${chain}: creating the pool"
    send "${npm}" "createAndInitializePoolIfNecessary(address,address,uint24,uint160)" \
      "${t0}" "${t1}" "${FEE}" "${sqrt}" --rpc-url "${rpc}"
    pool="$(cast call "${factory}" "getPool(address,address,uint24)(address)" "${t0}" "${t1}" "${FEE}" --rpc-url "${rpc}")"
  fi
  [ "${pool}" != "${ZERO}" ] || die "${chain}: no pool after creating it"
  save "pool-${chain}" "${pool}"

  if [ "$(cast call "${pool}" "liquidity()(uint128)" --rpc-url "${rpc}" | cut -d' ' -f1)" != 0 ]; then
    say "${chain}: pool ${pool} already has liquidity"
    continue
  fi
  say "${chain}: adding full-range liquidity"
  send "${t0}" "approve(address,uint256)" "${npm}" "${a0}" --rpc-url "${rpc}"
  send "${t1}" "approve(address,uint256)" "${npm}" "${a1}" --rpc-url "${rpc}"
  send "${npm}" "mint((address,address,uint24,int24,int24,uint256,uint256,uint256,uint256,address,uint256))" \
    "(${t0},${t1},${FEE},-${TICK},${TICK},${a0},${a1},$((a0 * 99 / 100)),$((a1 * 99 / 100)),${ME},$(( $(date +%s) + 1200 )))" \
    --rpc-url "${rpc}"
  say "${chain}: pool ${pool}, liquidity $(cast call "${pool}" "liquidity()(uint128)" --rpc-url "${rpc}")"
done <<< "${VENUES}"

say "pools ready"
