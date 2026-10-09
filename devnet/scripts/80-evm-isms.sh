#!/usr/bin/env bash
# Deploy a TeeDcapIsm on each EVM chain, pinned to the enclave this devnet just created.
#
# Every input is read live rather than from a checked-in constant, because all of them change
# when `make init` runs again: the enclave is a fresh CVM, and the origin checkpoint comes
# from a chain with a brand new genesis. That is why these are redeployed each cycle while the
# PCCS stack underneath them persists.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need cast
need forge

CONTRACTS="${REPO_DIR}/tee-hyperlane/contracts"
MAX_QUOTE_SKEW="${MAX_QUOTE_SKEW:-86400}"
: "${EVM_PRIVATE_KEY:?set EVM_PRIVATE_KEY}"

has merkle-hook-id || die "no local hyperlane deployment; run 'make init' first"

# ---------------------------------------------------------------- pin the live enclave

# Which enclave this ISM will trust. An EVM destination verifies a *Celestia*-origin
# attestation, so these ISMs pin the Celestia enclave; the Celestia-side ISMs pin whichever
# enclave attests their origin.
ENCLAVE="${ENCLAVE:-celestia}"
has "enclave-url-${ENCLAVE}" \
  || die "no ${ENCLAVE} enclave; run 'make init' first"
say "reading measurements from the devnet enclave"
curl -sS -m 30 "$(load "enclave-url-${ENCLAVE}")/identity" -o "${STATE_DIR}/enclave-identity.json"
MEASUREMENTS="$(python3 - "${STATE_DIR}/enclave-identity.json" <<'PY'
import json, subprocess, sys
q = json.load(open(sys.argv[1]))["quote"]
b = bytes.fromhex(q[2:] if q.startswith("0x") else q)[48:]
# mr_td ++ mr_config_id, then rtmr0..2. rtmr3 is excluded: it carries app-id and
# instance-id, so including it would tie the ISM to one CVM.
pre = "0x" + (b[136:232] + b[328:472]).hex()
print(subprocess.run(["cast", "keccak", pre], capture_output=True, text=True, check=True).stdout.strip())
PY
)"
[[ "${MEASUREMENTS}" =~ ^0x[0-9a-f]{64}$ ]] || die "could not compute the enclave's measurements from its quote (got '${MEASUREMENTS}')"
# The contract revision this checkout deploys, read from the source rather than repeated here.
ISM_REVISION="$(sed -n 's/.*uint8 public constant VERSION = \([0-9]*\);.*/\1/p' "${CONTRACTS}/src/TeeDcapIsm.sol")"
[ -n "${ISM_REVISION}" ] || die "no VERSION in TeeDcapIsm.sol"

IDENTITY="$(load "identity-digest-${ENCLAVE}")"
[ -n "${IDENTITY}" ] || die "no identity-digest-${ENCLAVE}; deploy that enclave first"
say "  measurements  ${MEASUREMENTS}"
say "  identity      ${IDENTITY}"

# ---------------------------------------------------------------- pin the local chain
#
# Where a new ISM starts:
#   - replacing a recorded ISM: from that ISM's last state, with only the identity (its last 32
#     bytes) swapped. The route resumes where the old one stopped, so nothing in flight is lost.
#     If the old state cannot be read the script stops rather than fall back to the head.
#   - no ISM recorded yet (a first deploy): at the chain's current head, computed here.
#   - ISM_GENESIS set: exactly that state, for every chain in CHAINS.
if [ -n "${ISM_GENESIS:-}" ]; then
  GENESIS="${ISM_GENESIS}"
  say "anchoring at the checkpoint in ISM_GENESIS, not at ${CHAINID}'s head"
else
  say "reading a trusted checkpoint from ${CHAINID}"
  write_config
  GENESIS="$("${COPROCESSOR_BIN}" --config "${COPROCESSOR_CONFIG}" genesis --chain celestia --identity "${IDENTITY}")"
fi
[ -n "${GENESIS}" ] || die "could not bootstrap from ${CHAINID}"
HOOK="$(load merkle-hook-id)"
say "  origin hook   ${HOOK}"

# ---------------------------------------------------------------- deploy
# chain : chain-id : hyperlane mailbox
# Eden's mailbox is ours: unlike the other three it has no canonical Hyperlane deployment,
# so `DeployHyperlaneCore` put one there.
# Override CHAINS to bring up a subset, which is what a partial or staged deployment needs:
#
#   CHAINS="sepolia:11155111:0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766" ./scripts/80-evm-isms.sh
#
# Each row is chain : chain-id : hyperlane mailbox.
CHAINS="${CHAINS:-arbitrum:421614:0x598facE78a4302f11E3de0bee1894Da0b2Cb71F8
base:84532:0x6966b0E55883d49BFB24539356a2f8A673E02039
sepolia:11155111:0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766
eden:3735928814:0x1D32350f3440BEa7f7E450Aa085f63E0d7E38729}"

# A here-string, not a pipe: `cmd | while` runs the loop in a subshell, so `die` only exits
# the subshell and `save` writes state the parent never sees. That is how this silently
# stopped after the second chain.
while IFS=: read -r name chainid mailbox; do
  addr_file="${OUT_DIR}/pccs-${name}.json"
  [ -f "${addr_file}" ] || { incomplete "${name}: no PCCS record at ${addr_file}; no ISM deployed there"; continue; }
  rpc="$(python3 -c "import json;print(json.load(open('${addr_file}'))['rpc'])")"
  entry="$(python3 -c "import json;print(json.load(open('${addr_file}'))['AttestationEntrypoint'])")"

  # Skip a chain whose ISM already pins this enclave and runs this revision of the contract.
  # Without this, re-running the step silently abandons the previous deployment and pays for
  # another. The revision matters too: a contract change with the same enclave (v5 quote
  # support, say) must still replace the ISM. The first deployments have no VERSION(), which
  # reads as empty and so as outdated.
  chain_genesis="${GENESIS}"
  if has "ism-${name}"; then
    existing="$(load "ism-${name}")"
    # A failed read must stop the run: taken as "different", it would replace a working ISM.
    cur="$(cast call "${existing}" "enclaveMeasurements()(bytes32)" --rpc-url "${rpc}" 2>&1)" \
      || die "could not read ${existing} on ${name}: ${cur}"
    # The first deployments have no VERSION(), and a call to it reverts: that means revision 1.
    rev="$(cast call "${existing}" "VERSION()(uint8)" --rpc-url "${rpc}" 2>/dev/null || true)"
    if [ "${cur}" = "${MEASUREMENTS}" ] && [ "${rev}" = "${ISM_REVISION}" ]; then
      say "${name} already has ${existing} pinning this enclave, skipping"
      continue
    fi
    # From revision 3 the owner re-pins in place: the ISM keeps its address and its state, so
    # no router has to be re-pointed. Only a contract change, or an ISM we do not own, replaces it.
    if [ "${rev}" = "${ISM_REVISION}" ] && [ "${ISM_REVISION}" -ge 3 ]; then
      owner="$(cast call "${existing}" "owner()(address)" --rpc-url "${rpc}" 2>&1)" \
        || die "could not read ${existing}'s owner on ${name}: ${owner}"
      me="$(cast wallet address --private-key "${EVM_PRIVATE_KEY}")"
      if [ "$(printf %s "${owner}" | tr A-F a-f)" = "$(printf %s "${me}" | tr A-F a-f)" ]; then
        say "${name}: re-pinning ${existing} to this enclave"
        cast send "${existing}" "setEnclave(bytes32,bytes32)" "${MEASUREMENTS}" "${IDENTITY}" \
          --rpc-url "${rpc}" --private-key "${EVM_PRIVATE_KEY}" >/dev/null \
          || die "setEnclave failed on ${name}"
        got="$(cast call "${existing}" "enclaveMeasurements()(bytes32)" --rpc-url "${rpc}" 2>/dev/null || true)"
        [ "${got}" = "${MEASUREMENTS}" ] || die "${existing} on ${name} pins '${got}' after the re-pin, expected ${MEASUREMENTS}"
        say "  ${existing} re-pinned"
        continue
      fi
      say "${name}: ${existing} is owned by ${owner}, not ${me}; replacing it"
    fi
    [ "${cur}" = "${MEASUREMENTS}" ] && say "${name}: ${existing} is contract revision ${rev:-1}, replacing with ${ISM_REVISION}"
    if [ -z "${ISM_GENESIS:-}" ]; then
      old="$(cast call "${existing}" "state()(bytes)" --rpc-url "${rpc}" 2>/dev/null || true)"
      [ "${#old}" -eq 234 ] || die "could not read ${existing}'s state on ${name}; refusing to anchor at the head and lose messages"
      chain_genesis="${old:0:170}${IDENTITY#0x}"
      say "${name}: resuming from ${existing}'s last state"
    fi
  fi

  say "deploying TeeDcapIsm on ${name}"
  out="$(cd "${CONTRACTS}" && forge create src/TeeDcapIsm.sol:TeeDcapIsm \
    --rpc-url "${rpc}" --private-key "${EVM_PRIVATE_KEY}" --broadcast \
    --constructor-args "${entry}" "${MEASUREMENTS}" "${IDENTITY}" "${HOOK}" \
                       "${mailbox}" "${chain_genesis}" "${MAX_QUOTE_SKEW}" 2>&1)"
  ism="$(printf '%s' "${out}" | grep -oE "Deployed to: 0x[0-9a-fA-F]{40}" | grep -oE "0x[0-9a-fA-F]{40}")"
  if [ -z "${ism}" ]; then
    printf '%s\n' "${out}" | tail -5 >&2
    die "TeeDcapIsm deployment failed on ${name}"
  fi
  save "ism-${name}" "${ism}"
  # Prove it is live and pinned to the enclave we just measured, before moving on.
  #
  # Retried: a read issued immediately after deployment can return empty while the node
  # catches up, and an empty answer here previously aborted the whole run after the second
  # chain, leaving the third undeployed and looking like success.
  got=""
  for _ in $(seq 1 10); do
    got="$(cast call "${ism}" "enclaveMeasurements()(bytes32)" --rpc-url "${rpc}" 2>/dev/null || true)"
    [ -n "${got}" ] && break
    sleep 3
  done
  [ "${got}" = "${MEASUREMENTS}" ] || die "deployed ISM on ${name} pins '${got}', expected ${MEASUREMENTS}"
  say "  ${ism} verified"
done <<< "${CHAINS}"

say "EVM ISMs deployed"
