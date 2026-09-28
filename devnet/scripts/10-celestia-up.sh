#!/usr/bin/env bash
# Build the celestia-app image if it is missing and bring the chain up.
#
# The image is rebuilt whenever it is absent, so pruning every image locally is a
# recoverable state rather than a broken devnet.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need docker
need go
need jq
need curl

ensure_image() {
  if docker image inspect "${CELESTIA_IMAGE}" >/dev/null 2>&1; then
    say "image ${CELESTIA_IMAGE} present"
    return 0
  fi
  [ -d "${CELESTIA_APP_DIR}" ] || die "no celestia-app checkout at ${CELESTIA_APP_DIR}; set CELESTIA_APP_DIR"
  say "building ${CELESTIA_IMAGE} from ${CELESTIA_APP_DIR} (this takes a few minutes)"
  docker build -t "${CELESTIA_IMAGE}" -f "${CELESTIA_APP_DIR}/docker/standalone.Dockerfile" "${CELESTIA_APP_DIR}"
}

ensure_image
ensure_binaries
ensure_mnemonic

mkdir -p "${STATE_DIR}/celestia"

say "starting ${CELESTIA_CONTAINER}"
STATE_DIR="${STATE_DIR}" CELESTIA_IMAGE="${CELESTIA_IMAGE}" CHAINID="${CHAINID}" \
  DEVNET_MNEMONIC="${DEVNET_MNEMONIC}" \
  docker compose -f "${DEVNET_DIR}/celestia/docker-compose.yml" up -d

wait_for_chain

mkdir -p "${OUT_DIR}"
save chain-id        "${CHAINID}"
save celestia-domain "${CELESTIA_DOMAIN}"
# Read first: a failure inside an argument does not stop the script, and would save an empty id.
relayer="$(addr relayer)"
user="$(addr user)"
save relayer-address "${relayer}"
save user-address    "${user}"
