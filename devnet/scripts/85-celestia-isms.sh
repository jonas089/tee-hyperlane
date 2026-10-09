#!/usr/bin/env bash
# The Celestia side of every route: one ISM per origin, and the routing ISM that fans them
# out.
#
# An ISM pins the identity of the enclave that attests *its* origin, and each origin has its
# own enclave image. In the shared CVM they all share one identity; a chain deployed alone has
# its own, and changing it leaves the other ISMs alone.
#
# Adding an origin is a row in ORIGINS; its genesis comes from its `[chains.<name>]` table.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need curl
wait_for_chain

A="${BIN_DIR}/celestia-appd"
TX="--from relayer --keyring-backend test --home ${CELHOME} --chain-id ${CHAINID}
    --node ${CELESTIA_RPC} --fees 200000utia --gas 900000 --broadcast-mode sync -y -o json"

# origin : domain : enclave : merkle tree address on that origin
#
# Override ORIGINS to bring up a subset, and keep it in step with CHAINS in 80-evm-isms.sh
# and 90-evm-warp.sh. An origin with an ISM here but no enrolled warp router from 90 produces
# a route that attests fine and then fails every delivery with "no enrolled router found for
# origin <domain>", forever.
ORIGINS="${ORIGINS:-sepolia:11155111:ethereum:0x0000000000000000000000004917a9746a7b6e0a57159ccb7f5a6744247f2d0d
arbitrum:421614:arbitrum:0x000000000000000000000000ad34a66bf6db18e858f6b686557075568c6e031c
base:84532:base:0x00000000000000000000000086fb9f1c124fb20ff130c41a79a432f770f67afd
eden:3735928814:eden:0x000000000000000000000000cfbe7016d123d52a7db4fc7d087ccb5421dbf8db}"

# Wait for a transaction and report its code, since `--broadcast-mode sync` only means the
# node accepted it.
settle() {
  local hash="$1" tries=0
  while [ "${tries}" -lt 12 ]; do
    sleep 3
    out="$("${A}" query tx "${hash}" --node "${CELESTIA_RPC}" -o json 2>/dev/null)" || { tries=$((tries+1)); continue; }
    printf '%s' "${out}"
    return 0
  done
  return 1
}

# send <what> <tx args...> - broadcast, wait, and stop with the node's own error on any failure,
# so a failed step is never mistaken for a finished one.
send() { # also leaves the included transaction in SENT, for callers that need its events
  local what="$1" out hash result; shift
  out="$("${A}" tx "$@" ${TX} 2>&1 || true)"
  hash="$(printf '%s' "${out}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["txhash"])' 2>/dev/null || true)"
  [ -n "${hash}" ] || die "${what}: not broadcast: ${out}"
  result="$(settle "${hash}" || true)"
  [ -n "${result}" ] || die "${what}: ${hash} not included"
  printf '%s' "${result}" | python3 -c 'import sys,json;d=json.load(sys.stdin);sys.exit(1 if d.get("code") else 0)' \
    || die "${what}: failed: $(printf '%s' "${result}" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("raw_log",""))')"
  SENT="${result}"
}

# The identity digest an ISM pins: the last 32 bytes of its state, which the module checks
# against the identity it stores.
pinned_identity() {
  "${A}" query teeism ism "$1" --node "${CELESTIA_RPC}" -o json 2>/dev/null | python3 -c '
import base64, json, sys
print(base64.b64decode(json.load(sys.stdin)["ism"]["state"])[-32:].hex())
' 2>/dev/null
}

# The account that owns an ISM, which alone may re-pin it.
ism_owner() {
  "${A}" query teeism ism "$1" --node "${CELESTIA_RPC}" -o json 2>/dev/null \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["ism"].get("owner",""))' 2>/dev/null
}

# The full state of an ISM, as 0x-hex.
ism_state() {
  "${A}" query teeism ism "$1" --node "${CELESTIA_RPC}" -o json 2>/dev/null | python3 -c '
import base64, json, sys
print("0x" + base64.b64decode(json.load(sys.stdin)["ism"]["state"]).hex())
' 2>/dev/null
}

# The genesis state for one origin. In order:
#   - ISM_GENESIS_<ORIGIN> if set: exactly that state.
#   - replacing a recorded ISM (<old id> given): that ISM's last state with only the identity,
#     its last 32 bytes, swapped. The route resumes where the old one stopped, so nothing in
#     flight is lost. An unreadable old state stops the script rather than fall back to the head.
#   - otherwise (a first deploy): the origin's current head, from the coprocessor config.
genesis_for() { # <origin> <enclave> [old ism id]
  local override old digest
  override="$(eval "printf '%s' \"\${ISM_GENESIS_$(printf '%s' "$1" | tr 'a-z-' 'A-Z_'):-}\"")"
  if [ -n "${override}" ]; then
    printf '%s' "${override}"
    return
  fi
  digest="$(load "identity-digest-$2" | sed 's/^0x//')"
  if [ -n "${3:-}" ]; then
    old="$(ism_state "$3")"
    [ "${#old}" -eq 234 ] || die "could not read $3's state; refusing to anchor $1 at the head and lose messages"
    printf '%s' "${old:0:170}${digest}"
    return
  fi
  "${COPROCESSOR_BIN}" --config "${COPROCESSOR_CONFIG}" genesis --chain "$1" --identity "0x${digest}"
}

write_config
RELAYER="$("${A}" keys show relayer -a --keyring-backend test --home "${CELHOME}")"

for row in ${ORIGINS}; do
  IFS=: read -r name domain enclave tree <<< "${row}"
  replacing=""
  say "== ${name} (domain ${domain}, ${enclave} enclave)"

  # Already created for this enclave, so leave it alone. An origin whose bootstrap failed the
  # first time is the normal reason to run this again - Eden's needs a synced DA node, which
  # can take half an hour - and without this the re-run mints a second ISM for every origin
  # that already worked. An ISM pinning an older identity is the other case: that is a
  # rotation, and it gets a new ISM, which the routing step below swaps in.
  if has "ism-celestia-${name}"; then
    existing="$(load "ism-celestia-${name}")"
    pinned="$(pinned_identity "${existing}")"
    # Unreadable is not the same as different: minting a replacement on a query hiccup would
    # re-point the route for nothing.
    [ -n "${pinned}" ] || die "could not read ${existing} from ${CELESTIA_RPC}"
    if [ "${pinned}" = "$(load "identity-digest-${enclave}" | tr 'A-F' 'a-f' | sed 's/^0x//')" ]; then
      say "  already created for this enclave: ${existing}"
      continue
    fi
    # The owner re-pins in place: the ISM keeps its id and its state, so the routing ISM is
    # untouched. ISM_GENESIS_<ORIGIN> asks for a new ISM at that state instead.
    override="$(eval "printf '%s' \"\${ISM_GENESIS_$(printf '%s' "${name}" | tr 'a-z-' 'A-Z_'):-}\"")"
    if [ -z "${override}" ] && [ "$(ism_owner "${existing}")" = "${RELAYER}" ]; then
      say "  ${existing} pins an older enclave; re-pinning it"
      send "re-pin ${name}" teeism update-identity "${existing}" "${OUT_DIR}/identity-${enclave}.json"
      [ "$(pinned_identity "${existing}")" = "$(load "identity-digest-${enclave}" | tr 'A-F' 'a-f' | sed 's/^0x//')" ] \
        || die "${existing} does not pin the new identity after the re-pin"
      continue
    fi
    say "  ${existing} pins an older enclave; creating a replacement from its last state"
    replacing="${existing}"
  fi

  # `|| true` is load-bearing. lib.sh sets `-euo pipefail`, so a bootstrap that exits non-zero
  # - base with no archive key, Eden with a DA node that has not caught up - kills the whole
  # script at this assignment, before the guard on the next line can skip that origin. The
  # guard read as if it handled the case and never once ran: base took the run down with it
  # and Eden, the origin after it, was never attempted.
  genesis="$(genesis_for "${name}" "${enclave}" "${replacing}" || true)"
  [ -n "${genesis}" ] || { incomplete "${name}: could not build its genesis state (the reason is above); its current ISM stays in place"; continue; }

  OUT_DIR="${OUT_DIR}" python3 - "${genesis}" "${tree}" "${name}" "${enclave}" <<'PY'
import json, sys, os
state, tree, name, enclave = sys.argv[1:5]
out = os.environ["OUT_DIR"]
json.dump({"state": state, "merkle_tree_address": tree,
           "identity": json.load(open(f"{out}/identity-{enclave}.json"))},
          open(f"{out}/ism-{name}-origin.json", "w"), indent=2)
PY

  out="$("${A}" tx teeism create "${OUT_DIR}/ism-${name}-origin.json" ${TX} 2>&1 || true)"
  hash="$(printf '%s' "${out}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["txhash"])' 2>/dev/null || true)"
  [ -n "${hash}" ] || { incomplete "${name}: ISM create not broadcast: ${out}"; continue; }
  id="$( (settle "${hash}" || true) | python3 -c '
import sys, json
d = json.load(sys.stdin)
if d.get("code"):
    print("", end="")
    raise SystemExit
for ev in d["events"]:
    if "teeism" in ev["type"]:
        for a in ev["attributes"]:
            if a["key"] == "id":
                print(a["value"].strip(chr(34)))
' 2>/dev/null || true)"
  [ -n "${id}" ] || { incomplete "${name}: ISM not created; see tx ${hash} (celestia-appd query tx ${hash})"; continue; }
  save "ism-celestia-${name}" "${id}"
  say "  ${id}"
done

# ---------------------------------------------------------------- routing
#
# Four origins deliver into one Celestia token and each ISM pins one origin domain, so a
# single ISM would reject three of four.
say "== routing ism"
# Reused, never re-created. This is the ISM the mailbox and both warp tokens point at, so a
# second one does not replace the first, it orphans it: the domains registered on the old one
# stay there and nothing points at it any more.
if has routing-ism-id; then
  routing="$(load routing-ism-id)"
  say "  already created: ${routing}"
else
  send "create the routing ism" hyperlane ism create-routing
  routing="$(printf '%s' "${SENT}" | python3 -c '
import sys, json
for ev in json.load(sys.stdin)["events"]:
    if "RoutingIsm" in ev["type"] or "routing" in ev["type"].lower():
        for a in ev["attributes"]:
            if a["key"] in ("ism_id", "id"):
                print(a["value"].strip(chr(34)))
                raise SystemExit
')"
  [ -n "${routing}" ] || die "routing ISM not created"
  save routing-ism-id "${routing}"
  say "  ${routing}"
fi

for row in ${ORIGINS}; do
  IFS=: read -r name domain _ _ <<< "${row}"
  has "ism-celestia-${name}" || continue
  say "  domain ${domain} -> $(load "ism-celestia-${name}")"
  # Remove, then set. On mocha-5 a set on a domain already present succeeded, emitted the
  # event naming the new ISM, and changed nothing. The version pinned here overwrites, but a
  # rotation has to hold on either, and removing an absent domain is a no-op.
  send "remove domain ${domain}" hyperlane ism remove-routing-ism-domain "${routing}" "${domain}"
  send "set domain ${domain}" hyperlane ism set-routing-ism-domain "${routing}" "${domain}" "$(load "ism-celestia-${name}")"
done

say "== pointing the tokens and the mailbox at it"
for key in celestia-token-id celestia-teeusd-token-id; do
  has "${key}" || continue
  send "point ${key} at the routing ism" warp set-token "$(load "${key}")" --ism-id "${routing}"
  say "  $(load "${key}")"
done
send "point the mailbox at the routing ism" hyperlane mailbox set "$(load mailbox-id)" --default-ism "${routing}"
say "  mailbox default"

say "celestia ISMs ready"
