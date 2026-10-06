#!/usr/bin/env bash
# Make every EVM chain's Automata entrypoint verify v5 TDX quotes as well as v4.
#
# Phala's hosts emit v4 quotes today. A host or TDX-module upgrade can switch them to v5, and an
# entrypoint without a v5 verifier then refuses every quote, which stops every route into that
# chain. TeeDcapIsm accepts both since VERSION 2, so this is the other half.
#
# Idempotent: a chain whose entrypoint already has a v5 verifier is left alone. Run before
# 80-evm-isms.sh. Needs the deploy key, which owns the entrypoint and the PCCS router.
#
#   CHAINS="eden" ./scripts/75-dcap-verifiers.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need forge
need cast
need jq
need git

CHAINS="${CHAINS:-sepolia arbitrum base eden}"
AUTOMATA="${REPO_DIR}/devnet/automata"
EVM="${AUTOMATA}/automata/evm"
# The only library V5QuoteVerifier needs beyond the vendored PCCS, at the revision
# pccs/foundry.lock pins.
SOLADY_REV=772a49d91b004eb55dc9129ae57dd36fbb8417ee
ZERO=0x0000000000000000000000000000000000000000

# Automata's submodules are not vendored. Link the PCCS we do vendor where their remappings look
# for it, and fetch solady. Both paths are gitignored.
prepared=0
prepare() {
  [ "${prepared}" = 1 ] && return
  [ -e "${EVM}/lib/automata-on-chain-pccs" ] \
    || { mkdir -p "${EVM}/lib" && ln -s ../../../pccs "${EVM}/lib/automata-on-chain-pccs"; }
  if [ ! -d "${AUTOMATA}/pccs/lib/solady/src" ]; then
    git clone -q https://github.com/Vectorized/solady "${AUTOMATA}/pccs/lib/solady"
    git -C "${AUTOMATA}/pccs/lib/solady" checkout -q "${SOLADY_REV}"
  fi
  prepared=1
}

send() { # <rpc> <to> <sig> <args...>
  local rpc="$1"; shift
  cast send "$@" --rpc-url "${rpc}" --private-key "${EVM_PRIVATE_KEY}" >/dev/null
}

for c in ${CHAINS}; do
  f="${OUT_DIR}/pccs-${c}.json"
  [ -f "${f}" ] || { incomplete "${c}: no PCCS record at ${f}; copy it or deploy Automata there (DEPLOY appendix C)"; continue; }
  rpc="$(jq -r .rpc "${f}")"
  entry="$(jq -r .AttestationEntrypoint "${f}")"
  router="$(jq -r .PCCSRouter "${f}")"

  current="$(cast call "${entry}" 'quoteVerifiers(uint16)(address)' 5 --rpc-url "${rpc}")"
  if [ "${current}" != "${ZERO}" ]; then
    say "${c}: v5 verifier already registered at ${current}"
    jq --arg a "${current}" '.V5QuoteVerifier = $a' "${f}" > "${f}.new" && mv "${f}.new" "${f}"
    continue
  fi

  # The same P256 verifier the chain's v4 verifier uses. On all four chains today that is the
  # RIP-7212 precompile at 0x…0100.
  p256="$(cast call "$(jq -r .V4QuoteVerifier "${f}")" 'P256_VERIFIER()(address)' --rpc-url "${rpc}")"
  [ -n "${p256}" ] && [ "${p256}" != "${ZERO}" ] || die "${c}: could not read the v4 verifier's P256 verifier"

  prepare
  say "${c}: deploying V5QuoteVerifier (p256 ${p256}, router ${router})"
  # --constructor-args last: it takes every argument after it, so a key placed after it never
  # reaches forge as the signer.
  out="$(cd "${EVM}" && forge create contracts/verifiers/V5QuoteVerifier.sol:V5QuoteVerifier \
    --rpc-url "${rpc}" --private-key "${EVM_PRIVATE_KEY}" --broadcast \
    --constructor-args "${p256}" "${router}" 2>&1)" \
    || { printf '%s\n' "${out}" | tail -5 >&2; die "${c}: V5QuoteVerifier deployment failed"; }
  verifier="$(printf '%s' "${out}" | grep -oE 'Deployed to: 0x[0-9a-fA-F]{40}' | grep -oE '0x[0-9a-fA-F]{40}')"
  [ -n "${verifier}" ] || die "${c}: no address in the deployment output"

  # The router serves collateral only to authorised callers, and the entrypoint picks the
  # verifier by the version it reports.
  send "${rpc}" "${router}" 'setAuthorized(address,bool)' "${verifier}" true
  send "${rpc}" "${entry}" 'setQuoteVerifier(address)' "${verifier}"

  registered="$(cast call "${entry}" 'quoteVerifiers(uint16)(address)' 5 --rpc-url "${rpc}")"
  [ "$(printf '%s' "${registered}" | tr 'A-F' 'a-f')" = "$(printf '%s' "${verifier}" | tr 'A-F' 'a-f')" ] \
    || die "${c}: entrypoint reports ${registered} for v5, not ${verifier}"
  jq --arg a "${verifier}" '.V5QuoteVerifier = $a' "${f}" > "${f}.new" && mv "${f}.new" "${f}"
  say "${c}: v5 verifier ${verifier} registered"
done
