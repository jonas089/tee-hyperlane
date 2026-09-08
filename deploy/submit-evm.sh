#!/usr/bin/env bash
# Submit one proved attestation to a TeeIsm on any EVM chain, then deliver its messages.
#
# Three transactions, in this order and no other:
#   updateState    advances the trusted origin state
#   submitMessages authorises the batch under the new root
#   process        hands each message addressed here to the Mailbox, which asks the ISM
#
# Only messages for this chain are processed. A batch necessarily contains every leaf in the
# range - that is what makes the merkle replay work - so on a shared origin it carries
# messages bound elsewhere. Handing one of those to the Mailbox reverts with "unexpected
# destination", and because the batch then never finishes, the route retries it forever.
#
# The order is forced: submitMessages checks the batch against the *current* root, so the
# state has to move first.
#
# Every step is skipped if the chain shows it already happened. That is what makes a crashed
# relayer safe to restart: re-running this script with the same proved.json finishes the
# batch instead of failing or double-spending gas. A batch abandoned half-submitted is the
# one way a message can be lost, because the trusted height has already moved past it.
set -euo pipefail

PROVED=${1:?usage: submit-evm.sh <proved.json>}
ISM=${TEE_ISM:?set TEE_ISM}
MAILBOX=${MAILBOX:?set MAILBOX}
RPC=${EVM_RPC:?set EVM_RPC}
# Asked of the mailbox when not supplied, so this script is correct on its own rather than
# only when its caller remembers to pass it.
# `cast` prints "11155111 [1.115e7]", so keep only the number.
LOCAL_DOMAIN=${LOCAL_DOMAIN:-$(cast call "$MAILBOX" "localDomain()(uint32)" --rpc-url "$RPC" | awk '{print $1}')}
PK=0x$(tr -d ' \n\r' < "$(dirname "$0")/../keys/SEPOLIA_PRIVATE_KEY.md")

jqv() { python3 -c "import sys,json;print(json.load(open('$PROVED'))$1)"; }

# The new state root is the first 32 bytes of the transition's *second* state, which the
# public values carry as u64_le(len) || state || u64_le(len) || new_state.
new_state_root() {
  python3 - "$PROVED" <<'PY'
import json, sys
pv = bytes.fromhex(json.load(open(sys.argv[1]))["proofs"]["state_transition"]["public_values"])
first = int.from_bytes(pv[0:8], "little")
second_at = 8 + first + 8
print("0x" + pv[second_at:second_at + 32].hex())
PY
}

send() {
  cast send "$@" --rpc-url "$RPC" --private-key "$PK" --json | python3 -c "
import sys, json
d = json.load(sys.stdin)
print('  status', d['status'], 'gas', int(d['gasUsed'], 16), 'tx', d['transactionHash'])
sys.exit(0 if int(d['status'], 16) == 1 else 1)"
}

WANT=$(new_state_root)

echo "== updateState =="
if [ "$(cast call "$ISM" "stateRoot()(bytes32)" --rpc-url "$RPC")" = "$WANT" ]; then
  echo "  state already at $WANT, skipping"
else
  send "$ISM" "updateState(bytes,bytes)" \
    "0x$(jqv "['proofs']['state_transition']['proof']")" \
    "0x$(jqv "['proofs']['state_transition']['public_values']")"
fi

echo "== submitMessages =="
# Ask whether a batch was submitted for this root, not whether some id is still authorised:
# `verify` deletes each id as it is consumed, so a delivered batch looks unsubmitted and the
# retry reverts with MessagesAlreadySubmitted.
if [ "$(cast call "$ISM" "messagesSubmittedForRoot()(bool)" --rpc-url "$RPC")" = "true" ]; then
  echo "  batch already submitted for this root, skipping"
else
  send "$ISM" "submitMessages(bytes,bytes)" \
    "0x$(jqv "['proofs']['state_membership']['proof']")" \
    "0x$(jqv "['proofs']['state_membership']['public_values']")"
fi

echo "== deliver messages addressed to domain $LOCAL_DOMAIN =="
COUNT=$(python3 -c "import json;print(len(json.load(open('$PROVED'))['messages']))")
for i in $(seq 0 $((COUNT-1))); do
  MSG=0x$(jqv "['messages'][$i]")
  if [ "$MSG" = "0x" ]; then echo "  [$i] no message bytes recorded, skipping"; continue; fi
  # Hyperlane lays the destination domain at bytes 41..45, so hex characters 82..90.
  DEST=$(python3 -c "print(int('${MSG#0x}'[82:90], 16))")
  if [ "$DEST" != "$LOCAL_DOMAIN" ]; then
    echo "  [$i] destination $DEST, not ours - skipping"
    continue
  fi
  ID=$(cast keccak "$MSG")
  if [ "$(cast call "$MAILBOX" "delivered(bytes32)(bool)" "$ID" --rpc-url "$RPC")" = "true" ]; then
    echo "  [$i] $ID already delivered"; continue
  fi
  echo "  [$i] processing $ID"
  send "$MAILBOX" "process(bytes,bytes)" 0x "$MSG"
done
