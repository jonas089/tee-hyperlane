#!/usr/bin/env bash
# What is deployed, and where.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

row() { printf '  %-22s %s\n' "$1" "$2"; }

echo
echo "chain"
if height="$(curl -s -m 2 "${CELESTIA_RPC}/status" 2>/dev/null | jq -r '.result.sync_info.latest_block_height // empty')"; then
  row "status" "up, height ${height:-unknown}"
else
  row "status" "down"
fi
row "chain id" "${CHAINID}"
row "rpc"      "${CELESTIA_RPC}"
row "rest"     "${CELESTIA_API}"
row "domain"   "${CELESTIA_DOMAIN}"

echo
echo "hyperlane"
for k in mailbox-id merkle-hook-id noop-ism-id noop-hook-id; do
  has "$k" && row "$k" "$(load "$k")"
done

echo
echo "tee ism"
for c in ${ENCLAVE_CHAINS}; do
  has "enclave-url-${c}" && row "${c} enclave" "$(load "enclave-url-${c}")"
  has "identity-digest-${c}" && row "${c} identity" "$(load "identity-digest-${c}")"
done
has ism-celestia-sepolia && row ism-celestia-sepolia "$(load ism-celestia-sepolia)"
if has ism-celestia-sepolia; then
  state="$(q teeism ism "$(load ism-celestia-sepolia)" 2>/dev/null \
    | python3 -c "import sys,json,base64;print(base64.b64decode(json.load(sys.stdin)['ism']['state']).hex())" 2>/dev/null)"
  if [ -n "${state}" ]; then
    row "state root"  "0x${state:0:64}"
    row "origin head" "$((16#${state:72:16}))"
  fi
fi

echo
echo "warp"
for k in celestia-token-id; do
  has "$k" && row "$k" "$(load "$k")"
done

echo
echo "accounts"
for k in relayer-address user-address; do
  if has "$k"; then
    a="$(load "$k")"
    bal="$(q bank balances "$a" 2>/dev/null | jq -r '[.balances[]?|select(.denom=="utia")|.amount]|first // "0"')"
    row "$k" "${a} (${bal}utia)"
  fi
done
echo
