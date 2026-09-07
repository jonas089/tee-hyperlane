#!/usr/bin/env bash
# Deploy a Celestia-origin TeeIsm and a synthetic TIA router on an EVM testnet.
#
# The Celestia enclave already attests Celestia; a new destination needs no new enclave and
# no new circuits, only its own ISM with its own genesis state. That is what "two enclaves
# cover four networks" means in practice.
set -euo pipefail

CHAIN=${1:?usage: deploy-l2-ism.sh <arbitrum|base>}
ROOT=$(cd "$(dirname "$0")/.." && pwd)

case "$CHAIN" in
  arbitrum)
    RPC=${ARBITRUM_SEPOLIA_RPC:-https://arbitrum-sepolia-rpc.publicnode.com}
    MAILBOX=0x598facE78a4302f11E3de0bee1894Da0b2Cb71F8
    ;;
  base)
    RPC=${BASE_SEPOLIA_RPC:-https://base-sepolia-rpc.publicnode.com}
    MAILBOX=0x6966b0E55883d49BFB24539356a2f8A673E02039
    ;;
  *) echo "unknown chain $CHAIN" >&2; exit 1 ;;
esac

: "${IDENTITY_DIGEST:?set IDENTITY_DIGEST (xtask vkeys)}"
: "${STATE_TRANSITION_VKEY:?}" "${STATE_MEMBERSHIP_VKEY:?}"
PK=0x$(tr -d ' \n\r' < "$ROOT/keys/SEPOLIA_PRIVATE_KEY.md")

# Celestia's merkle tree hook is the origin for all three EVM destinations.
export ORIGIN_MERKLE_TREE=0x726f757465725f706f73745f6469737061746368000000030000000000000000
export MAX_STATE_AGE=${MAX_STATE_AGE:-86400}

echo "== anchoring the ISM to a live Celestia header =="
GENESIS=$("$ROOT/tee-hyperlane/target/release/tee-hyperlane" bootstrap-celestia \
  --identity-digest "$IDENTITY_DIGEST" | awk '/genesis state/ {print $3}')
echo "   $GENESIS"
export GENESIS_STATE=$GENESIS

cd "$ROOT/tee-hyperlane/contracts"
echo "== TeeIsm on $CHAIN =="
ISM=$(forge script script/DeployTeeIsm.s.sol:DeployTeeIsm --rpc-url "$RPC" \
  --private-key "$PK" --broadcast --slow 2>&1 | awk '/TeeIsm  /{print $2}')
echo "   $ISM"

echo "== synthetic TIA on $CHAIN =="
MAILBOX=$MAILBOX TEE_ISM=$ISM ORIGIN_DOMAIN=1297040200 \
ORIGIN_ROUTER=0x726f757465725f61707000000000000000000000000000010000000000000000 \
forge script script/DeployWarpSynthetic.s.sol:DeployWarpSynthetic --rpc-url "$RPC" \
  --private-key "$PK" --broadcast --slow 2>&1 | grep -E "HypERC20|ism "

echo
echo "Next: enroll this router on Celestia so the route works both ways:"
echo "  celestia-appd tx warp enroll-remote-router <tia-token-id> <domain> <router-bytes32> 50000"
