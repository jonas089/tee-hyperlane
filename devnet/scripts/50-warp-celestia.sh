#!/usr/bin/env bash
# Create the Celestia side of the warp routes.
#
# Two assets, deliberately pointing opposite ways, because lock/unlock and mint/burn are
# different code paths and one asset only exercises half of each:
#
#   TIA   Celestia native. Collateral here, synthetic on every EVM chain.
#         Sending locks here and mints there; receiving burns there and unlocks here.
#   USDC  EVM native. Synthetic here, collateral on Sepolia.
#         Sending burns here and unlocks there; receiving locks there and mints here.
#   teeUSD Ours. Synthetic everywhere, with a fixed supply minted once here (below).
#
# Carrying both means a change that breaks one direction cannot pass unnoticed.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

wait_for_chain

# Point a token at the routing ISM explicitly rather than relying on the mailbox default, so
# that adding another ISM later cannot silently change what secures the route.
#
# The routing ISM, never one origin's. This pointed at `ism-celestia-sepolia` before the
# per-family split, which was the same thing when there was one origin and is wrong now: a
# token secured by the Sepolia-origin ISM rejects every Eden-origin transfer into it. Only
# reachable if 85-celestia-isms.sh ran first, which in `make init` it does not, so the guard
# below is the normal path and 85 does the pointing itself.
point_at_ism() {
  local token="$1"
  has routing-ism-id || return 0
  say "pointing ${token} at the routing ism"
  tx relayer warp set-token "${token}" --ism-id "$(load routing-ism-id)" >/dev/null \
    || incomplete "${token}: could not point it at the routing ism (the error is above); 85-celestia-isms.sh does it again"
}

if has celestia-token-id; then
  say "TIA token already created: $(load celestia-token-id)"
else
  say "creating the TIA collateral token"
  res="$(tx relayer warp create-collateral-token "$(load mailbox-id)" utia)"
  token="$(ev "${res}" "hyperlane.warp.v1.EventCreateCollateralToken" "token_id")"
  [ -n "${token}" ] || die "could not read the TIA token id"
  save celestia-token-id "${token}"
  point_at_ism "${token}"
fi

if has celestia-usdc-token-id; then
  say "USDC token already created: $(load celestia-usdc-token-id)"
else
  say "creating the USDC synthetic token"
  res="$(tx relayer warp create-synthetic-token "$(load mailbox-id)")"
  token="$(ev "${res}" "hyperlane.warp.v1.EventCreateSyntheticToken" "token_id")"
  [ -n "${token}" ] || die "could not read the USDC token id"
  save celestia-usdc-token-id "${token}"
  point_at_ism "${token}"
  # A synthetic's denom only exists once the token does, so record it rather than expecting
  # anyone downstream to reconstruct it. This is the string the UI's CELESTIA_DENOM needs.
  save celestia-usdc-denom "hyperlane/${token}"
fi

# teeUSD: a dollar token we issue, so trading pairs can be seeded without anyone else's asset.
#
# There is no mint module, so the supply arrives the way any synthetic's does: as a Hyperlane
# message. The token briefly trusts the noop ISM and a made-up origin, takes one message that
# mints TEEUSD_SUPPLY to the holder, then drops the made-up origin and moves to the routing
# ISM. With that origin gone no message can mint again, so the supply is fixed. Each step
# checks the chain first, so a run interrupted anywhere is finished by running it again.
TEEUSD_SUPPLY="${TEEUSD_SUPPLY:-1000000000000000}"   # 1B at 6 decimals
TEEUSD_HOLDER="${TEEUSD_HOLDER:-$(addr user)}"
MINT_DOMAIN=1952802133                                # "teeU", no real chain
MINT_ROUTER=0x0000000000000000000000000000000000000000000000000000000074656555

if has celestia-teeusd-token-id; then
  say "teeUSD token already created: $(load celestia-teeusd-token-id)"
else
  say "creating the teeUSD synthetic token"
  res="$(tx relayer warp create-synthetic-token "$(load mailbox-id)")"
  token="$(ev "${res}" "hyperlane.warp.v1.EventCreateSyntheticToken" "token_id")"
  [ -n "${token}" ] || die "could not read the teeUSD token id"
  save celestia-teeusd-token-id "${token}"
  save celestia-teeusd-denom "hyperlane/${token}"
fi
token="$(load celestia-teeusd-token-id)"
denom="$(load celestia-teeusd-denom)"
mint_enrolled() {
  q warp remote-routers "${token}" | jq -e --argjson d "${MINT_DOMAIN}" \
    '[.remote_routers[]? | select(.receiver_domain == $d)] | length > 0' >/dev/null
}

supply="$(q bank total-supply-of "${denom}" | jq -r '.amount.amount // .amount // "0"')"
if [ "${supply}" = 0 ]; then
  say "minting ${TEEUSD_SUPPLY} ${denom} to ${TEEUSD_HOLDER}"
  tx relayer warp set-token "${token}" --ism-id "$(load noop-ism-id)" >/dev/null
  mint_enrolled || tx relayer warp enroll-remote-router "${token}" "${MINT_DOMAIN}" "${MINT_ROUTER}" 0 >/dev/null
  holder="$(appd debug addr "${TEEUSD_HOLDER}" | sed -n 's/^Address (hex): //p' | tr 'A-F' 'a-f')"
  [ ${#holder} = 40 ] || die "could not decode ${TEEUSD_HOLDER}"
  # version 3 | nonce 0 | origin | sender | destination | recipient | body (recipient, amount)
  msg="$(printf '0x03%08x%08x%s%08x%s%064s%064x' 0 "${MINT_DOMAIN}" "${MINT_ROUTER#0x}" \
    "${CELESTIA_DOMAIN}" "${token#0x}" "${holder}" "${TEEUSD_SUPPLY}" | tr ' ' 0)"
  tx relayer hyperlane mailbox process "$(load mailbox-id)" 0x "${msg}" >/dev/null
  supply="$(q bank total-supply-of "${denom}" | jq -r '.amount.amount // .amount // "0"')"
  [ "${supply}" = "${TEEUSD_SUPPLY}" ] || die "teeUSD supply is ${supply} after the mint, expected ${TEEUSD_SUPPLY}"
fi
if mint_enrolled; then
  say "dropping the mint origin; the teeUSD supply is now fixed at ${supply}"
  tx relayer warp unroll-remote-router "${token}" "${MINT_DOMAIN}" >/dev/null
fi
point_at_ism "${token}"

# Remote routers are enrolled by 90-evm-warp.sh, which owns both directions of each route.
# They cannot be enrolled here: the EVM routers do not exist yet at this point in `make init`.

say "celestia warp side ready"
