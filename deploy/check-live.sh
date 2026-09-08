#!/usr/bin/env bash
# Does the code in this checkout produce the enclave the ISMs will accept a proof from?
#
# Three links, each checked against the thing downstream of it rather than against this
# repo's own claims:
#
#   local source -> image digest -> compose hash -> what the live enclaves measure -> vkeys
#
# Pass --rebuild to also rebuild the image from source (~35 min) and compare its digest.
# Without it the image leg is checked against the registry, which is fast and still proves
# the compose file pins a digest that exists and that the enclaves booted it.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
GATEWAY=dstack-pha-prod9.phala.network
ETH=16882cc467b8d243f0a98ba686d214b910902e02
CEL=f6231f5329c6c70a8236045504ab28a9b7e782bb
fail=0
ok()   { printf '  \033[32mOK\033[0m   %s\n' "$1"; }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=1; }

echo "1. docker image the compose file pins"
PINNED=$(grep -oE 'sha256:[0-9a-f]{64}' "$ROOT/deploy/docker-compose.yml")
echo "     pinned    $PINNED"
CONFIG=$(docker manifest inspect "ghcr.io/jonas089/tee-node@$PINNED" 2>/dev/null \
         | python3 -c 'import sys,json;print(json.load(sys.stdin)["config"]["digest"])' 2>/dev/null)
[ -n "$CONFIG" ] && ok "registry serves it (config ${CONFIG:7:16}...)" \
                 || bad "registry does not serve that digest"

if [ "${1:-}" = "--rebuild" ]; then
  echo "   rebuilding from source, this takes about 35 minutes..."
  ( cd "$ROOT" && nix build .#image ) || bad "nix build failed"
  BUILT=$(tar -xOf "$ROOT/result" manifest.json | python3 -c 'import sys,json;print(json.load(sys.stdin)[0]["Config"].replace(".json",""))')
  echo "     rebuilt   sha256:$BUILT"
  [ "sha256:$BUILT" = "$CONFIG" ] && ok "local source reproduces the deployed image" \
                                  || bad "local source does NOT reproduce it"
fi

echo "2. what the two live enclaves actually measure"
PINNED_ID="$ROOT/tee-circuit/tee-attestation/enclave-identity.toml"
for app in "$ETH" "$CEL"; do
  live=$(cd "$ROOT/tee-circuit" && cargo run -q -p circuit-tool -- identity \
           --url "https://$app-8080.$GATEWAY" 2>/dev/null | grep -v '^#')
  [ -z "$live" ] && { bad "${app:0:10}... unreachable"; continue; }
  if diff -q <(echo "$live") <(grep -v '^#' "$PINNED_ID") >/dev/null; then
    ok "${app:0:10}... measures exactly what this checkout pins"
  else
    bad "${app:0:10}... differs from the pinned identity"
    diff <(echo "$live") <(grep -v '^#' "$PINNED_ID") | sed 's/^/       /'
  fi
done
echo "     compose_hash $(grep compose_hash "$PINNED_ID" | cut -d'"' -f2)"

echo "3. vkeys the ISMs were created with"
read -r ST SM < <(cd "$ROOT/tee-circuit" && cargo run -q -p circuit-tool -- vkeys 2>/dev/null \
                  | awk '/state-transition/{a=$3} /state-membership/{b=$3} END{print a, b}')
echo "     local     $ST"
for row in "sepolia https://ethereum-sepolia-rpc.publicnode.com 0x552c240a658f663EdeB5138346a406055eA0e55b" \
           "arbitrum https://arbitrum-sepolia-rpc.publicnode.com 0xf87b6f53058824a1Ec30C2c3c5961184e947043D" \
           "base https://base-sepolia-rpc.publicnode.com 0x13A28e9E6077cA8ebe0c097864c830f1159905cd"; do
  set -- $row
  a=$(cast call "$3" "stateTransitionVkey()(bytes32)" --rpc-url "$2" 2>/dev/null)
  b=$(cast call "$3" "stateMembershipVkey()(bytes32)" --rpc-url "$2" 2>/dev/null)
  [ "$a" = "$ST" ] && [ "$b" = "$SM" ] && ok "$1 ISM carries these vkeys" || bad "$1 ISM carries different vkeys"
done

echo
[ $fail -eq 0 ] && echo "All links hold: this checkout is what the ISMs trust." \
                || echo "Something does not line up - see the FAILs above."
exit $fail
