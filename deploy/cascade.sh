#!/usr/bin/env bash
# Re-pin the whole stack after the enclave's code changes.
#
# One thing changing forces all of it, in one direction:
#
#   source -> image digest -> compose hash -> RTMR3 -> enclave identity -> vkeys -> ISMs
#
# An ISM's vkeys are immutable, so a new identity cannot be adopted by an existing ISM; it
# needs new ones, and the warp routers have to be pointed at them. Nothing is reused except
# the mailboxes, the merkle tree hooks, the IGPs and the warp routers themselves, none of
# which depend on the enclave.
#
# Run the stages in order. Each is idempotent on its own and prints what the next one needs,
# so a stage that fails can be re-run without unwinding the ones before it.
#
#   deploy/cascade.sh image      # nix build, push, pin the digest into docker-compose.yml
#   deploy/cascade.sh enclaves   # deploy both CVMs, re-pin identity, rebuild circuits
#   deploy/cascade.sh isms       # new ISMs on all four chains, routers re-pointed
#   deploy/cascade.sh relayer    # rebuild and restart the coprocessor
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
STAGE=${1:?usage: cascade.sh <image|enclaves|isms|relayer>}

: "${ARK:=chef@178.199.12.26}"
IMAGE_REPO=ghcr.io/jonas089/tee-node
GATEWAY=dstack-pha-prod9.phala.network
CELESTIA_HOOK=0x726f757465725f706f73745f6469737061746368000000030000000000000000
ROUTING_ISM=0x726f757465725f69736d0000000000000000000000000001000000000000000c
CEL_NODE=https://rpc-mocha.pops.one
CEL_CHAIN=mocha-4

# domain:rpc:mailbox:tia-router:usdc-router
EVM_CHAINS=(
  "11155111:https://ethereum-sepolia-rpc.publicnode.com:0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766:0xFeA14C1444A7a8beAb7122fdE5A168212D7185bE:0xfb611B6f6CE92033960e99C2D65cee4237e64cDD"
  "421614:https://arbitrum-sepolia-rpc.publicnode.com:0x598facE78a4302f11E3de0bee1894Da0b2Cb71F8:0xFeA14C1444A7a8beAb7122fdE5A168212D7185bE:0xb9E5E3eb926EA22B951d2fb7392F9F3D6c704054"
  "84532:https://base-sepolia-rpc.publicnode.com:0x6966b0E55883d49BFB24539356a2f8A673E02039:0xf4197C55C944987E9b10e09C0A47915211769B78:0x0ee6374a92ba4E11F920A23c6dd271b594D69A9B"
)

case "$STAGE" in

image)
  # Nix rather than `docker build`, so the digest is something a reader can reproduce
  # instead of something they have to take on trust.
  echo "== building the enclave image on $ARK =="
  rsync -a --delete --exclude target --exclude .git --exclude node_modules \
    --exclude result --exclude context "$ROOT/" "$ARK:~/tee-isms/"
  ssh "$ARK" 'bash ~/nixbuild.sh'
  ssh "$ARK" 'tail -1 ~/nix.log'
  DIGEST=$(ssh "$ARK" 'docker load -i ~/tee-isms/result 2>/dev/null | sed -n "s/.*sha256:/sha256:/p"' | tail -1)
  echo "   $DIGEST"
  echo
  echo "Next: push it and pin it."
  echo "  ssh $ARK docker tag $DIGEST $IMAGE_REPO:reproducible"
  echo "  ssh $ARK docker push $IMAGE_REPO:reproducible"
  echo "  sed -i '' \"s#image: .*#image: $IMAGE_REPO@\$DIGEST#\" deploy/docker-compose.yml"
  ;;

enclaves)
  # Two CVMs on one compose file. The app id and instance id differ and are deliberately
  # not pinned; the compose hash is what the circuits bind, which is why one identity
  # covers both and why an enclave can be replaced without touching an ISM.
  echo "== deploy both CVMs on deploy/docker-compose.yml, then set ETH_APP and CEL_APP =="
  : "${ETH_APP:?app id of the Ethereum-origin CVM}" "${CEL_APP:?app id of the Celestia-origin CVM}"
  cd "$ROOT/tee-circuit"
  cargo run -q -p circuit-tool -- identity --url "https://$ETH_APP-8080.$GATEWAY" --write
  cargo run -q -p circuit-tool -- build
  cargo run -q -p circuit-tool -- vkeys | tee /tmp/vkeys.txt
  echo
  echo "== the second CVM must measure the same, or the pinning is per-instance =="
  diff <(cargo run -q -p circuit-tool -- identity --url "https://$ETH_APP-8080.$GATEWAY") \
       <(cargo run -q -p circuit-tool -- identity --url "https://$CEL_APP-8080.$GATEWAY") \
    && echo "   both enclaves measure identically"
  ;;

isms)
  : "${IDENTITY_DIGEST:?}" "${STATE_TRANSITION_VKEY:?}" "${STATE_MEMBERSHIP_VKEY:?}"
  BIN="$ROOT/tee-hyperlane/target/release/tee-hyperlane"
  PK=0x$(tr -d ' \n\r' < "$ROOT/keys/SEPOLIA_PRIVATE_KEY.md")
  export ORIGIN_MERKLE_TREE=$CELESTIA_HOOK MAX_STATE_AGE=${MAX_STATE_AGE:-86400}
  export STATE_TRANSITION_VKEY STATE_MEMBERSHIP_VKEY

  # Celestia-origin ISMs, one per EVM destination. Each anchors to a live Celestia header;
  # anything dispatched before that header is outside the new ISM's history and has to be
  # re-sent, so this is the moment to be sure nothing is mid-flight.
  for entry in "${EVM_CHAINS[@]}"; do
    IFS=: read -r domain rpc mailbox tia usdc <<<"$entry"
    echo "== domain $domain =="
    export GENESIS_STATE MAILBOX=$mailbox
    GENESIS_STATE=$("$BIN" bootstrap-celestia --identity-digest "$IDENTITY_DIGEST" \
      | awk '/genesis state/ {print $3}')
    ISM=$(cd "$ROOT/tee-hyperlane/contracts" && forge script \
      script/DeployTeeIsm.s.sol:DeployTeeIsm --rpc-url "$rpc" --private-key "$PK" \
      --broadcast --slow 2>&1 | awk '/TeeIsm  /{print $2}')
    echo "   TeeIsm $ISM"
    for router in "$tia" "$usdc"; do
      cast send "$router" "setInterchainSecurityModule(address)" "$ISM" \
        --rpc-url "$rpc" --private-key "$PK" >/dev/null
      echo "   $router -> $ISM"
    done
  done

  # The other direction: one zkism instance per EVM origin, hung off the routing ISM the
  # warp tokens already point at. Updating the routing table in place is what keeps the
  # Celestia-side warp config untouched.
  echo "== Celestia =="
  for entry in "${EVM_CHAINS[@]}"; do
    IFS=: read -r domain _ <<<"$entry"
    echo "   create a zkism for origin $domain, then:"
    echo "   celestia-appd tx hyperlane ism set-routing-ism-domain $ROUTING_ISM $domain <new-ism-id> \\"
    echo "     --from deployer --chain-id $CEL_CHAIN --node $CEL_NODE --fees 20000utia -y"
  done
  ;;

relayer)
  echo "== rebuilding the coprocessor on $ARK =="
  rsync -a --delete --exclude target --exclude .git --exclude node_modules \
    --exclude result --exclude context "$ROOT/" "$ARK:~/tee-isms/"
  ssh "$ARK" 'bash ~/rebuild.sh; tail -1 ~/build.log'
  echo "Then update coprocessor.toml with the new ism_id and tee_node_url per route, and:"
  echo "  ssh $ARK sudo systemctl restart tee-hyperlane"
  ;;

*) echo "unknown stage $STAGE" >&2; exit 1 ;;
esac
