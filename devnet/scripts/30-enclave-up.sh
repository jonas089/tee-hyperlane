#!/usr/bin/env bash
# Deploy the enclaves and wait for each to answer.
#
#   ./scripts/30-enclave-up.sh                     # every chain, in one shared CVM
#   ./scripts/30-enclave-up.sh --ethereum --base   # each named chain alone, in its own CVM
#
# One image per origin chain either way; the shared CVM runs them all side by side, each on
# its own port (see `enclave_port` in lib.sh). A chain deployed alone keeps its port, so the
# coprocessor only ever needs a different app id in the URL.
#
# The enclaves are the one piece that cannot be local: a TDX quote has to come from real Intel
# hardware. They measure the same compose files the testnet measures, because the identity an
# ISM pins is a property of the code, not of which CVM runs it.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need phala
need curl

# `all`, or the chains named on the command line, each its own CVM.
TARGETS=""
for arg in "$@"; do
  chain="${arg#--}"
  enclave_port "${chain}" >/dev/null || die "unknown chain ${arg}; use --<chain> with one of: ${ENCLAVE_CHAINS}"
  TARGETS="${TARGETS} ${chain}"
done
TARGETS="${TARGETS:-all}"

# The chains a target serves.
chains_in() { [ "$1" = all ] && echo "${ENCLAVE_CHAINS}" || echo "$1"; }

# A chain's URL in the CVM with this app id.
url_for() { echo "https://$1-$(enclave_port "$2").dstack-pha-prod9.phala.network"; }

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

deploy_cvm() {
  local target="$1"
  local compose
  compose="$(compose_file "${target}")"
  local name="teeism-${target}"
  local first url chain
  first="$(chains_in "${target}" | awk '{print $1}')"

  [ -f "${compose}" ] || die "no compose for ${target} at ${compose}"

  # Keep a recorded CVM only if it answers *and* measured this compose file. Answering is not
  # enough: after an image is re-pinned the old CVM is still healthy, and keeping it would
  # leave every ISM deployed next pinning the old code.
  if has "enclave-app-id-${target}"; then
    url="$(url_for "$(load "enclave-app-id-${target}")" "${first}")"
    if curl -sf -m 15 "${url}/health" >/dev/null 2>&1; then
      local measured
      measured="$(measured_compose "${url}" || true)"
      # Unreadable is not the same as different: deploying a new CVM over a failed read would
      # replace a working enclave for nothing.
      [ -n "${measured}" ] || die "${target} CVM ${url} answers /health but its /identity could not be read"
      if [ "${measured}" = "$(cat "${compose}")" ]; then
        say "${target} CVM already up"
        record_urls "${target}" "$(load "enclave-app-id-${target}")"
        return 0
      fi
      warn "recorded ${target} CVM runs a different compose; deploying a new one"
      warn "the old CVM keeps running; delete it with 'phala cvms delete' once nothing uses it"
    else
      warn "recorded ${target} CVM is not answering; deploying a new one"
    fi
    # The old CVM still holds the plain name, and Phala refuses a second CVM with it.
    name="${name}-$(date -u +%m%d-%H%M)"
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
    --wait --json 2>&1)" || { printf '%s\n' "${out}" >&2; die "phala deploy failed for ${target}"; }

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
    die "could not read the app id for ${target}; check 'phala cvms ls' for a stray ${name}"
  fi
  save "enclave-app-id-${target}" "${app_id}"

  local i
  for chain in $(chains_in "${target}"); do
    url="$(url_for "${app_id}" "${chain}")"
    say "waiting for ${url}"
    for i in $(seq 1 60); do
      curl -sf -m 10 "${url}/health" >/dev/null 2>&1 && break
      [ "${i}" -lt 60 ] || die "${chain} enclave did not come up; check 'phala cvms get --cvm-id ${app_id}'"
      sleep 10
    done
    say "${chain} enclave is answering"
  done
  record_urls "${target}" "${app_id}"
}

# Point every chain this CVM serves at it. A chain deployed alone later takes over its own.
record_urls() { # <target> <app id>
  local chain
  for chain in $(chains_in "$1"); do
    save "enclave-url-${chain}" "$(url_for "$2" "${chain}")"
  done
}

# Refuse to deploy an image the repo does not describe, or that the current code would not
# build. For each chain deployed:
#   - its source still evaluates to the store path deploy/images.lock recorded (new code with
#     an old image, or the reverse, fails here);
#   - its compose file pins the digest the lock recorded (a hand-edited compose fails here);
#   - docker-compose.all.yml is exactly what the per-chain files generate;
#   - all of them are committed and pushed, so `main` names what the enclaves run.
# FORCE_UNPINNED=1 skips all of it, for a local experiment and nothing else.
check_pins() {
  if [ "${FORCE_UNPINNED:-0}" = 1 ]; then
    warn "FORCE_UNPINNED=1: deploying without checking the pins against the code"
    return
  fi
  need nix
  need git
  local target chain out
  for target in ${TARGETS}; do
    for chain in $(chains_in "${target}"); do
      out="$(image_out_path "${chain}")" || die "could not evaluate .#image-${chain}"
      [ -n "$(locked "${chain}" out)" ] \
        || die "${chain} has no entry in deploy/images.lock; run scripts/25-images.sh"
      [ "$(locked "${chain}" out)" = "${out}" ] \
        || die "${chain}: the code changed since its image was pinned; run scripts/25-images.sh"
      [ "$(locked "${chain}" digest)" = "$(pinned_digest "${chain}")" ] \
        || die "${chain}: deploy/docker-compose.${chain}.yml does not pin the digest in deploy/images.lock"
    done
  done
  [ "$(compose_all)" = "$(cat "$(compose_file all)")" ] \
    || die "deploy/docker-compose.all.yml is not what the per-chain files generate; run scripts/25-images.sh"
  git -C "${REPO_DIR}" ls-files --error-unmatch deploy/images.lock >/dev/null 2>&1 \
    || die "deploy/images.lock is not committed; commit and push it first"
  git -C "${REPO_DIR}" diff --quiet HEAD -- deploy/images.lock 'deploy/docker-compose.*.yml' \
    || die "deploy/images.lock or a compose file has uncommitted changes; commit and push them first"
  git -C "${REPO_DIR}" fetch -q \
    && git -C "${REPO_DIR}" merge-base --is-ancestor HEAD '@{upstream}' \
    || die "this commit is not pushed; push it first, so the repo names what the enclaves run"
}

check_pins
for target in ${TARGETS}; do
  deploy_cvm "${target}"
done

# Each ISM pins the identity of the enclave that attests its origin, so record them now rather
# than re-reading them in every script that needs one. Read from the quote, and only after the
# event log replays to the RTMRs the hardware signed. Chains sharing a CVM share an identity.
(cd "${REPO_DIR}/tee-hyperlane" && cargo build --quiet --release -p tee-coprocessor)
for target in ${TARGETS}; do
  for chain in $(chains_in "${target}"); do
    url="$(load "enclave-url-${chain}")"
    "${COPROCESSOR_BIN}" identity --url "${url}" --json "${OUT_DIR}/identity-${chain}.json" \
      || die "could not read ${chain} identity from ${url}"
    digest="$("${BIN_DIR}/teeism-identity" -identity "${OUT_DIR}/identity-${chain}.json")"
    [ -n "${digest}" ] || die "no identity digest for ${chain}"
    save "identity-digest-${chain}" "${digest}"
    say "${chain} identity ${digest}"
  done
done
