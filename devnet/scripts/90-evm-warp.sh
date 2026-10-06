#!/usr/bin/env bash
# Deploy the EVM side of every warp route on every chain, and enroll both directions.
#
# This runs after 80-evm-isms.sh because a router's security module is set at initialize
# time and never changed afterwards: the ISM has to exist before the router that points at
# it. Both are redeployed on every `make init`, since the ISM pins a fresh enclave and a
# fresh chain genesis, and a router left pointing at the previous cycle's ISM would accept
# nothing.
#
# Enrollment lives here rather than in 50-warp-celestia.sh so that one step owns both
# directions of one route. Splitting it across two steps meant the Celestia half ran before
# the EVM half existed, so it silently enrolled nothing.
#
# Two assets run in opposite directions, which is the point of carrying both:
#
#   TIA   Celestia native  -> synthetic on all three EVM chains
#   USDC  EVM native       -> collateral on Sepolia, synthetic on the two L2s
#
# teeUSD rides along as a third, synthetic everywhere; its supply is minted on Celestia.
#
# Adding a third asset is a row in TOKENS plus its per-chain kind, and nothing else.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need cast
need forge

CONTRACTS="${REPO_DIR}/tee-hyperlane/contracts"
: "${EVM_PRIVATE_KEY:?set EVM_PRIVATE_KEY}"

# Destination gas for a warp delivery. Quoted by the origin hook, which is the noop hook on
# this devnet, so the value is recorded but never charged.
WARP_DEST_GAS="${WARP_DEST_GAS:-50000}"

# Circle's own testnet USDC on Sepolia. The collateral router wraps it, so this is the one
# address here that is not ours and must not be redeployed.
SEPOLIA_USDC_ERC20="${SEPOLIA_USDC_ERC20:-0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238}"

wait_for_chain

DOMAIN="$(load celestia-domain)"

# chain : chain-id : hyperlane mailbox
# Override CHAINS to bring up a subset, which is what a partial or staged deployment needs:
#
#   CHAINS="sepolia:11155111:0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766" ./scripts/90-evm-warp.sh
#
# Each row is chain : chain-id : hyperlane mailbox.
CHAINS="${CHAINS:-arbitrum:421614:0x598facE78a4302f11E3de0bee1894Da0b2Cb71F8
base:84532:0x6966b0E55883d49BFB24539356a2f8A673E02039
sepolia:11155111:0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766
eden:3735928814:0x1D32350f3440BEa7f7E450Aa085f63E0d7E38729}"

# label : celestia token state key : state key suffix : name : symbol : decimals
TOKENS="tia:celestia-token-id:router:Celestia TIA:TIA:6
usdc:celestia-usdc-token-id:usdc-router:USD Coin:USDC:6
teeusd:celestia-teeusd-token-id:teeusd-router:tee USD:teeUSD:6"

# Which shape each asset takes on a given chain. Everything is a synthetic except the chain
# the asset is actually native to, where the router escrows the real ERC20.
kind_for() {
  case "$1:$2" in
    usdc:sepolia) echo collateral ;;
    *)            echo synthetic  ;;
  esac
}

# Reuse a router if it is real code, repointing it at the current ISM when it has drifted.
#
# Checking the code size and not just the saved address matters: a previous run recorded an
# address that a simulated deployment had predicted but never broadcast, and every later step
# trusted it.
#
# Repointing rather than redeploying matters more. A router's ISM is not fixed at initialize
# time as this once assumed - `MailboxClient` lets the owner change it - and redeploying a
# **collateral** router abandons the escrow inside it. An identity rotation did exactly that
# to Sepolia's USDC router once, stranding real USDC in a contract nothing referenced any
# more. A synthetic is less dramatic but still orphans the supply it minted.
#
# So a recorded router is replaced only when the chain says there is no code at its address.
# Anything else that goes wrong - an RPC error, a router owned by someone else, a repoint that
# does not take - stops the run, because deploying over it is the one mistake that costs funds.
lower() { printf '%s' "$1" | tr 'A-Z' 'a-z'; }
reusable() { # <router> <rpc> <ism>: 0 reused or repointed, 1 no code there; dies otherwise
  local cand="$1" rpc="$2" ism="$3" code cur owner me out
  code="$(cast code "${cand}" --rpc-url "${rpc}" 2>&1)" \
    || die "could not read the code at ${cand} (${rpc}): ${code}"
  [ "${code}" = "0x" ] && return 1
  cur="$(cast call "${cand}" "interchainSecurityModule()(address)" --rpc-url "${rpc}" 2>&1)" \
    || die "could not read ${cand}'s ISM: ${cur}"
  [ "$(lower "${cur}")" = "$(lower "${ism}")" ] && return 0
  owner="$(cast call "${cand}" "owner()(address)" --rpc-url "${rpc}" 2>&1)" \
    || die "could not read ${cand}'s owner: ${owner}"
  me="$(cast wallet address --private-key "${EVM_PRIVATE_KEY}")"
  [ "$(lower "${owner}")" = "$(lower "${me}")" ] \
    || die "${cand} is owned by ${owner}, not ${me}; its owner has to point it at ${ism}. Not deploying a replacement."
  say "repointing ${cand} at ${ism}"
  out="$(cast send "${cand}" "setInterchainSecurityModule(address)" "${ism}" \
    --rpc-url "${rpc}" --private-key "${EVM_PRIVATE_KEY}" 2>&1)" \
    || die "repointing ${cand} failed: $(printf '%s' "${out}" | tail -3)"
  sleep 4
  cur="$(cast call "${cand}" "interchainSecurityModule()(address)" --rpc-url "${rpc}" 2>&1)" \
    || die "could not read ${cand}'s ISM after repointing: ${cur}"
  [ "$(lower "${cur}")" = "$(lower "${ism}")" ] \
    || die "${cand} still reports ISM ${cur} after repointing to ${ism}"
}

# `forge script` prints the address it simulated, which exists whether or not the broadcast
# succeeded, so the printed address alone is not evidence that anything landed.
confirm_code() {
  local addr="$1" rpc="$2" code=""
  for _ in $(seq 1 15); do
    code="$(cast code "${addr}" --rpc-url "${rpc}" 2>/dev/null || true)"
    [ -n "${code}" ] && [ "${code}" != "0x" ] && return 0
    sleep 3
  done
  return 1
}

# A here-string, not a pipe: a piped `while` runs in a subshell, where `die` exits only the
# subshell and `save` writes state the parent never sees.
while IFS=: read -r name chainid mailbox; do
  addr_file="${OUT_DIR}/pccs-${name}.json"
  [ -f "${addr_file}" ] || { incomplete "${name}: no PCCS record, so no ISM and no routers there"; continue; }
  rpc="$(python3 -c "import json;print(json.load(open('${addr_file}'))['rpc'])")"
  has "ism-${name}" || { incomplete "${name}: no ISM recorded; run 80-evm-isms.sh first"; continue; }
  ism="$(load "ism-${name}")"

  while IFS=: read -r label token_key suffix tname tsymbol tdec; do
    has "${token_key}" || { incomplete "${name}: no ${label} token on celestia; run 50-warp-celestia.sh first"; continue; }
    token="$(load "${token_key}")"
    kind="$(kind_for "${label}" "${name}")"
    key="${name}-${suffix}"

    router=""
    if has "${key}" && reusable "$(load "${key}")" "${rpc}" "${ism}"; then
      router="$(load "${key}")"
      say "${name} already has the ${label} router ${router} on this ISM, skipping"
    fi

    if [ -z "${router}" ]; then
      say "deploying the ${kind} ${label} router on ${name}"
      if [ "${kind}" = collateral ]; then
        out="$(cd "${CONTRACTS}" && MAILBOX="${mailbox}" TEE_ISM="${ism}" \
          COLLATERAL_TOKEN="${SEPOLIA_USDC_ERC20}" \
          forge script script/DeployWarpCollateral.s.sol:DeployWarpCollateral \
            --rpc-url "${rpc}" --private-key "${EVM_PRIVATE_KEY}" --broadcast --slow 2>&1)"
        router="$(printf '%s' "${out}" | sed -n 's/.*HypERC20Collateral.*\(0x[0-9a-fA-F]\{40\}\).*/\1/p' | tail -1)"
      else
        out="$(cd "${CONTRACTS}" && MAILBOX="${mailbox}" TEE_ISM="${ism}" \
          ORIGIN_DOMAIN="${DOMAIN}" ORIGIN_ROUTER="${token}" \
          TOKEN_NAME="${tname}" TOKEN_SYMBOL="${tsymbol}" TOKEN_DECIMALS="${tdec}" \
          forge script script/DeployWarpSynthetic.s.sol:DeployWarpSynthetic \
            --rpc-url "${rpc}" --private-key "${EVM_PRIVATE_KEY}" --broadcast --slow 2>&1)"
        router="$(printf '%s' "${out}" | sed -n 's/.*HypERC20 *\(0x[0-9a-fA-F]\{40\}\).*/\1/p' | tail -1)"
      fi
      if [ -z "${router}" ]; then
        printf '%s\n' "${out}" | tail -20 >&2
        die "${label} router deployment failed on ${name}"
      fi
      confirm_code "${router}" "${rpc}" \
        || die "no code at ${router} on ${name}; the broadcast did not land"
      save "${key}" "${router}"
      save "warp-${label}-${name}" "${router}"
      sleep 4
    fi

    # EVM -> Celestia. Idempotent on its own: enrolling the same domain twice overwrites.
    got="$(cast call "${router}" "routers(uint32)(bytes32)" "${DOMAIN}" --rpc-url "${rpc}" 2>/dev/null || true)"
    if [ "${got}" != "${token}" ]; then
      say "enrolling the celestia ${label} token on ${name}"
      # Paced, not fired back to back. Two sends in the same second race the node's nonce
      # tracking and the second comes back "replacement transaction underpriced", which reads
      # like a rejected enrolment rather than a collision.
      for attempt in 1 2 3; do
        if out="$(cast send "${router}" "enrollRemoteRouter(uint32,bytes32)" "${DOMAIN}" "${token}" \
             --rpc-url "${rpc}" --private-key "${EVM_PRIVATE_KEY}" 2>&1)"; then
          break
        fi
        [ "${attempt}" = 3 ] && die "enrollRemoteRouter failed for ${label} on ${name}: $(printf '%s' "${out}" | tail -3)"
        warn "enrollRemoteRouter attempt ${attempt} failed, retrying: $(printf '%s' "${out}" | tail -1)"
        sleep $((attempt * 6))
      done
      sleep 4
      got="$(cast call "${router}" "routers(uint32)(bytes32)" "${DOMAIN}" --rpc-url "${rpc}" 2>/dev/null || true)"
    fi
    [ "${got}" = "${token}" ] || die "${name} ${label} router points at '${got}', expected ${token}"

    # A router with no hook dispatches messages nothing ever delivers. A new one takes the
    # hook the chain's TIA router already uses.
    hook="$(cast call "${router}" "hook()(address)" --rpc-url "${rpc}")"
    if [ "${hook}" = 0x0000000000000000000000000000000000000000 ] && [ "${key}" != "${name}-router" ] && has "${name}-router"; then
      want="$(cast call "$(load "${name}-router")" "hook()(address)" --rpc-url "${rpc}")"
      if [ "${want}" != 0x0000000000000000000000000000000000000000 ]; then
        say "setting the ${label} router's hook to ${want}"
        cast send "${router}" "setHook(address)" "${want}" \
          --rpc-url "${rpc}" --private-key "${EVM_PRIVATE_KEY}" >/dev/null
        sleep 4
      fi
    fi
    say "  ${label} ${router} ism ${ism}"
  done <<< "${TOKENS}"
done <<< "${CHAINS}"

# ---------------------------------------------------------------- Celestia -> EVM
#
# Read what the chain actually holds rather than a saved marker. The markers went stale the
# first time a router address changed underneath them, and the chain rejects enrolling a
# domain that is already enrolled, so a repoint has to unroll first.
enroll_from_celestia() {
  local token="$1" domain="$2" name="$3" label="$4" key="$5"
  has "${key}" || { incomplete "celestia ${label} token: no ${name} router to enroll"; return 0; }
  local router want got
  router="$(load "${key}")"
  want="0x000000000000000000000000$(printf '%s' "${router#0x}" | tr 'A-Z' 'a-z')"
  got="$(appd query warp remote-routers "${token}" --node "${CELESTIA_RPC}" -o json 2>/dev/null \
    | jq -r --argjson d "${domain}" '.remote_routers[]? | select(.receiver_domain == $d) | .receiver_contract' \
    | tr 'A-Z' 'a-z')"

  if [ "${got}" = "${want}" ]; then
    say "${name} ${label} already enrolled on domain ${domain}"
    return 0
  fi
  if [ -n "${got}" ]; then
    say "repointing ${label} domain ${domain} from ${got}"
    tx relayer warp unroll-remote-router "${token}" "${domain}" >/dev/null
  fi
  # The 32-byte form, not the 20-byte address: Hyperlane addresses are 32 bytes everywhere,
  # and this CLI rejects a bare address rather than padding it.
  say "enrolling ${name} ${label} router ${router} on domain ${domain}"
  tx relayer warp enroll-remote-router "${token}" "${domain}" "${want}" "${WARP_DEST_GAS}" >/dev/null
  save "enrolled-${label}-${domain}" "${router}"
}

while IFS=: read -r label token_key suffix tname tsymbol tdec; do
  has "${token_key}" || continue
  token="$(load "${token_key}")"
  enroll_from_celestia "${token}" "${SEPOLIA_DOMAIN}"          sepolia  "${label}" "sepolia-${suffix}"
  enroll_from_celestia "${token}" "${BASE_SEPOLIA_DOMAIN}"     base     "${label}" "base-${suffix}"
  enroll_from_celestia "${token}" "${ARBITRUM_SEPOLIA_DOMAIN}" arbitrum "${label}" "arbitrum-${suffix}"
  enroll_from_celestia "${token}" "${EDEN_DOMAIN}" eden "${label}" "eden-${suffix}"
done <<< "${TOKENS}"

say "evm warp side ready"
