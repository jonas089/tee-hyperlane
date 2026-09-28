#!/usr/bin/env bash
# Deploy one enclave per origin family and wait for each to answer.
#
# The enclaves are the one piece that cannot be local: a TDX quote has to come from real Intel
# hardware, and this machine is not it. Everything else in this devnet runs on the laptop.
#
# Three families, three images, three CVMs. They measure the same compose files the testnet
# measures, because the identity an ISM pins is a property of the code, not of which CVM runs
# it - app id and instance id are deliberately outside the measurement. Deploying from a
# separate devnet compose would only produce a fourth image nothing can rebuild.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need phala
need curl

FAMILIES="${FAMILIES:-celestia ethereum evolve}"
INSTANCE_TYPE="${INSTANCE_TYPE:-tdx.small}"
# Node 18 is prod9. Auto-selection sometimes lands on prod5, whose teepod reports
# tproxy_base_domain: None - the CVM runs, the gateway never registers it, and every request
# terminates TLS then returns nothing.
NODE_ID="${PHALA_NODE_ID:-18}"
OS_IMAGE="${PHALA_OS_IMAGE:-dstack-0.5.9}"

authenticated=0

# The compose file an enclave measured, from the app_compose dstack reports. The same document
# `deploy/verify-digest.sh` checks against the signed quote.
measured_compose() {
  curl -sf -m 30 "$1/identity" 2>/dev/null | python3 -c '
import json, sys
tcb = json.load(sys.stdin)["info"]["tcb_info"]
tcb = json.loads(tcb) if isinstance(tcb, str) else tcb
sys.stdout.write(json.loads(tcb["app_compose"])["docker_compose_file"])
' 2>/dev/null
}

deploy_family() {
  local family="$1"
  local compose="${REPO_DIR}/deploy/docker-compose.${family}.yml"
  local name="teeism-${family}"
  local url

  [ -f "${compose}" ] || die "no compose for ${family} at ${compose}"

  # Keep a recorded enclave only if it answers *and* measured this compose file. Answering is
  # not enough: after an image is re-pinned the old CVM is still healthy, and keeping it would
  # leave every ISM this family deploys next pinning the old code.
  if has "enclave-url-${family}"; then
    url="$(load "enclave-url-${family}")"
    if curl -sf -m 15 "${url}/health" >/dev/null 2>&1; then
      if [ "$(measured_compose "${url}")" = "$(cat "${compose}")" ]; then
        say "${family} enclave already up at ${url}"
        return 0
      fi
      warn "recorded ${family} enclave ${url} runs a different compose; deploying a new one"
      warn "the old CVM keeps running; delete it with 'phala cvms delete' once nothing uses it"
    else
      warn "recorded ${family} enclave ${url} is not answering; deploying a new one"
    fi
  fi

  # Checked once, and only when something actually needs deploying, so a devnet whose three
  # enclaves are all healthy does not require Phala credentials at all.
  if [ "${authenticated}" -eq 0 ]; then
    phala status >/dev/null 2>&1 || die "not authenticated to Phala Cloud. Run 'phala auth login <api-key>' with a key from cloud.phala.network > Settings > API Keys"
    authenticated=1
  fi

  say "deploying ${name} (${INSTANCE_TYPE}, node ${NODE_ID}, ${OS_IMAGE})"
  # --no-dev-os matters: if the CLI finds an SSH public key on this machine it otherwise
  # provisions a dev image that permits shell access into the CVM, which would make the
  # enclave's measurements meaningless.
  local out
  out="$(phala deploy \
    --name "${name}" \
    --compose "${compose}" \
    --instance-type "${INSTANCE_TYPE}" \
    --node-id "${NODE_ID}" \
    --image "${OS_IMAGE}" \
    --no-dev-os \
    --wait --json 2>&1)" || { printf '%s\n' "${out}" >&2; die "phala deploy failed for ${family}"; }

  # The CLI prints progress before its JSON and wraps the payload differently per command, so
  # the id is pulled from the first JSON object in the output rather than from a fixed path.
  local app_id
  app_id="$(printf '%s' "${out}" | python3 -c "
import json, re, sys
raw = sys.stdin.read()
for m in re.finditer(r'[{\[]', raw):
    try:
        doc, _ = json.JSONDecoder().raw_decode(raw[m.start():])
    except ValueError:
        continue
    stack = [doc]
    while stack:
        node = stack.pop()
        if isinstance(node, dict):
            for key in ('app_id', 'appId'):
                if isinstance(node.get(key), str) and node[key]:
                    print(node[key]); raise SystemExit
            stack.extend(node.values())
        elif isinstance(node, list):
            stack.extend(node)
" 2>/dev/null | head -1)"
  if [ -z "${app_id}" ]; then
    # The deploy may still have created a CVM, so say where to look rather than leaving one
    # billing quietly.
    printf '%s\n' "${out}" >&2
    die "could not read the app id for ${family}; check 'phala cvms ls' for a stray ${name}"
  fi

  url="https://${app_id}-8080.dstack-pha-prod9.phala.network"
  save "enclave-app-id-${family}" "${app_id}"
  save "enclave-url-${family}"    "${url}"

  say "waiting for ${url}"
  local i
  for i in $(seq 1 60); do
    if curl -sf -m 10 "${url}/health" >/dev/null 2>&1; then
      say "${family} enclave is answering"
      return 0
    fi
    sleep 10
  done
  die "${family} enclave did not come up; check 'phala cvms get --cvm-id ${app_id}'"
}

# Refuse to deploy an image the repo does not describe, or that the current code would not
# build. For each family:
#   - its source still evaluates to the store path deploy/images.lock recorded (new code with
#     an old image, or the reverse, fails here);
#   - its compose file pins the digest the lock recorded (a hand-edited compose fails here);
#   - both files are committed and pushed, so `main` names what the enclave runs.
# FORCE_UNPINNED=1 skips all of it, for a local experiment and nothing else.
check_pins() {
  if [ "${FORCE_UNPINNED:-0}" = 1 ]; then
    warn "FORCE_UNPINNED=1: deploying without checking the pins against the code"
    return
  fi
  need nix
  need git
  local family out
  for family in ${FAMILIES}; do
    out="$(image_out_path "${family}")" || die "could not evaluate .#image-${family}"
    [ -n "$(locked "${family}" out)" ] \
      || die "${family} has no entry in deploy/images.lock; run scripts/25-images.sh"
    [ "$(locked "${family}" out)" = "${out}" ] \
      || die "${family}: the code changed since its image was pinned; run scripts/25-images.sh"
    [ "$(locked "${family}" digest)" = "$(pinned_digest "${family}")" ] \
      || die "${family}: deploy/docker-compose.${family}.yml does not pin the digest in deploy/images.lock"
  done
  git -C "${REPO_DIR}" ls-files --error-unmatch deploy/images.lock >/dev/null 2>&1 \
    || die "deploy/images.lock is not committed; commit and push it first"
  git -C "${REPO_DIR}" diff --quiet HEAD -- deploy/images.lock 'deploy/docker-compose.*.yml' \
    || die "deploy/images.lock or a compose file has uncommitted changes; commit and push them first"
  git -C "${REPO_DIR}" fetch -q \
    && git -C "${REPO_DIR}" merge-base --is-ancestor HEAD '@{upstream}' \
    || die "this commit is not pushed; push it first, so the repo names what the enclaves run"
}

check_pins
for family in ${FAMILIES}; do
  deploy_family "${family}"
done

# Each ISM pins the identity of the family that attests its origin, so record all three now
# rather than re-reading them in every script that needs one. Read from the quote, and only
# after the event log replays to the RTMRs the hardware signed.
(cd "${REPO_DIR}/tee-hyperlane" && cargo build --quiet --release -p tee-coprocessor)
for family in ${FAMILIES}; do
  url="$(load "enclave-url-${family}")"
  "${COPROCESSOR_BIN}" identity --url "${url}" --json "${OUT_DIR}/identity-${family}.json" \
    || die "could not read ${family} identity from ${url}"
  digest="$("${BIN_DIR}/teeism-identity" -identity "${OUT_DIR}/identity-${family}.json")"
  [ -n "${digest}" ] || die "no identity digest for ${family}"
  save "identity-digest-${family}" "${digest}"
  say "${family} identity ${digest}"
done
