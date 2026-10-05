#!/usr/bin/env bash
# Build, push and pin each chain's enclave image.
#
# The only step that writes deploy/docker-compose.*.yml and deploy/images.lock, so they cannot
# drift apart and 30-enclave-up.sh can check the pins against the code. A chain whose source
# still evaluates to its locked store path is left alone: no build, no push.
# docker-compose.all.yml is regenerated from the per-chain files every run.
#
#   CHAINS="eden" ./scripts/25-images.sh      # one chain
#
# Needs x86_64 Linux, nix, and docker logged in to ghcr.io with write:packages.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need nix
need docker

CHAINS="${CHAINS:-${ENCLAVE_CHAINS}}"
IMAGE=ghcr.io/jonas089/tee-node
touch "${IMAGES_LOCK}"
pinned=""

for c in ${CHAINS}; do
  enclave_port "${c}" >/dev/null || die "no enclave image for ${c}; the chains are: ${ENCLAVE_CHAINS}"
  out="$(image_out_path "${c}")"
  if [ "$(locked "${c}" out)" = "${out}" ] && [ "$(locked "${c}" digest)" = "$(pinned_digest "${c}")" ]; then
    say "${c}: unchanged, still ${out##*/}"
    continue
  fi

  say "${c}: building ${out##*/}"
  (cd "${REPO_DIR}" && nix build ".#image-${c}" -o "result-${c}")
  [ "$(readlink -f "${REPO_DIR}/result-${c}")" = "${out}" ] || die "${c}: nix built something other than ${out}"
  docker load < "${REPO_DIR}/result-${c}" >/dev/null
  # The digest the registry assigns, read off the push itself: the one Phala pulls and the
  # compose hash covers.
  digest="$(docker push "${IMAGE}:reproducible-${c}" | grep -oE 'digest: sha256:[0-9a-f]{64}' | cut -d' ' -f2 || true)"
  [ -n "${digest}" ] || die "${c}: push did not report a digest"

  sed -i "s|tee-node@sha256:[0-9a-f]*|tee-node@${digest}|" "$(compose_file "${c}")"
  {
    grep '^#' "${IMAGES_LOCK}" || true
    { grep -v -e '^#' -e "^${c} " "${IMAGES_LOCK}" || true; printf '%s %s %s\n' "${c}" "${out}" "${digest}"; } | sort
  } > "${IMAGES_LOCK}.new" && mv "${IMAGES_LOCK}.new" "${IMAGES_LOCK}"
  say "${c}: pinned ${digest}"
  pinned="${pinned} ${c}"
done

compose_all > "$(compose_file all)"

if [ -n "${pinned}" ]; then
  echo
  say "pinned:${pinned}"
  say "commit and push deploy/docker-compose.*.yml and deploy/images.lock, then run 30-enclave-up.sh"
fi
