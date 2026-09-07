#!/usr/bin/env bash
# Submit one proved attestation to Celestia's x/zkism, then deliver its messages.
#
# Same three steps as the EVM side, because the ISM is the same protocol:
#   update           advances the trusted origin state
#   submit-messages  authorises the batch under the new root
#   mailbox process  hands each message to the mailbox, which asks the ISM
#
# Only messages addressed to this chain are processed. The batch necessarily contains every
# leaf inserted in the range - that is what makes the merkle replay check work - so on a
# shared mailbox it will include other people's messages. They are authorised but never
# delivered here, because their destination domain is not ours.
#
# Every step is skipped if the chain shows it already happened, so re-running this script
# with the same proved.json finishes an interrupted batch instead of failing.
set -euo pipefail

PROVED=${1:?usage: submit-celestia.sh <proved.json>}
ISM=${CELESTIA_ISM:?set CELESTIA_ISM}
MAILBOX=${CELESTIA_MAILBOX:-0x68797065726c616e650000000000000000000000000000000000000000000000}
NODE=${CELESTIA_RPC:-https://rpc-mocha.pops.one}
HOME_DIR=${CELHOME:-/tmp/celhome}
LOCAL_DOMAIN=${CELESTIA_DOMAIN:-1297040200}
APPD=${APPD:-celestia-appd}

TX="--home $HOME_DIR --keyring-backend test --chain-id mocha-5 --node $NODE --from bridge -y -o json"
Q="--node $NODE -o json"

jqv() { python3 -c "import sys,json;print(json.load(open('$PROVED'))$1)"; }

# A tx that reports code 0 in CheckTx can still fail in DeliverTx, so wait for the result.
send() {
  local hash
  hash=$("$APPD" tx $@ $TX --fees 10000utia --gas 800000 \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['txhash'])")
  echo "  tx $hash"
  for _ in $(seq 1 20); do
    sleep 3
    if out=$("$APPD" query tx "$hash" $Q 2>/dev/null); then
      python3 -c "
import sys, json
d = json.loads(sys.argv[1])
print('  code', d['code'], 'height', d['height'], d.get('raw_log','')[:160])
sys.exit(0 if d['code'] == 0 else 1)" "$out"
      return
    fi
  done
  echo "  timed out waiting for $hash" >&2
  return 1
}

WANT=$(python3 - "$PROVED" <<'PY'
import json, sys
pv = bytes.fromhex(json.load(open(sys.argv[1]))["proofs"]["state_transition"]["public_values"])
first = int.from_bytes(pv[0:8], "little")
print("0x" + pv[8 + first + 8:8 + first + 8 + 32].hex())
PY
)

state_root() {
  "$APPD" query zkism ism "$ISM" $Q 2>/dev/null \
    | python3 -c "import sys,json,base64;print('0x'+base64.b64decode(json.load(sys.stdin)['ism']['state'])[:32].hex())"
}

echo "== update state =="
if [ "$(state_root || echo none)" = "$WANT" ]; then
  echo "  state already at $WANT, skipping"
else
  send zkism update "$ISM" "$(jqv "['proofs']['state_transition']['proof']")" \
                           "$(jqv "['proofs']['state_transition']['public_values']")"
fi

echo "== submit messages =="
FIRST=$(jqv "['batch'][0]")
if "$APPD" query zkism messages "$ISM" $Q 2>/dev/null | grep -qi "${FIRST#0x}"; then
  echo "  batch already authorised, skipping"
else
  send zkism submit-messages "$ISM" "$(jqv "['proofs']['state_membership']['proof']")" \
                                    "$(jqv "['proofs']['state_membership']['public_values']")"
fi

echo "== deliver messages addressed to domain $LOCAL_DOMAIN =="
COUNT=$(python3 -c "import json;print(len(json.load(open('$PROVED'))['messages']))")
for i in $(seq 0 $((COUNT-1))); do
  MSG=$(jqv "['messages'][$i]")
  [ -z "$MSG" ] && continue
  DEST=$(python3 -c "print(int('$MSG'[82:90],16))")
  if [ "$DEST" != "$LOCAL_DOMAIN" ]; then
    echo "  [$i] destination $DEST, not ours - skipping"
    continue
  fi
  ID=$(jqv "['batch'][$i]")
  if "$APPD" query hyperlane delivered "$MAILBOX" "$ID" $Q 2>/dev/null | grep -q '"delivered": *true'; then
    echo "  [$i] $ID already delivered"; continue
  fi
  echo "  [$i] processing $ID"
  send hyperlane mailbox process "$MAILBOX" "0x" "0x$MSG"
done
