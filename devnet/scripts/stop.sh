#!/usr/bin/env bash
# Tear the devnet down and leave nothing behind.
#
# Deliberately destructive: the chain is meant to start from a fresh genesis every time, so
# keeping state between runs would make a run depend on the one before it.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

KEEP_BIN="${KEEP_BIN:-1}"

say "stopping the coprocessor"
# The crate is tee-coprocessor but the binary it builds is tee-hyperlane, so matching on the
# crate name silently matched nothing and left the relayer running.
pkill -f "tee-hyperlane --config" 2>/dev/null || true
pkill -f "vite.*3000" 2>/dev/null || true

# The shared CVM and any chain deployed alone. All are deleted, because a CVM bills by the
# hour and leaving one behind is the single mistake here that costs money.
for target in all ${ENCLAVE_CHAINS}; do
  has "enclave-app-id-${target}" || continue
  app_id="$(load "enclave-app-id-${target}")"
  say "deleting the ${target} phala cvm ${app_id}"
  phala cvms delete --cvm-id "${app_id}" --force 2>&1 | tail -2 \
    || warn "could not delete ${app_id}; it is still billing, so check 'phala cvms ls'"
done

say "stopping the chain"
STATE_DIR="${STATE_DIR}" docker compose -f "${DEVNET_DIR}/celestia/docker-compose.yml" down -v 2>&1 | tail -3 || true

say "pruning state"
# Three things under .state are not chain state and must survive.
#
# The PCCS address records: those contracts live on public testnets and outlive any number of
# stop/init cycles. Losing them means redeploying seventeen contracts per chain to recover
# addresses that are still perfectly good.
#
# The hand-written route config. coprocessor.toml and gas-oracle.toml take three steps of
# DEPLOY.md to fill in and nothing regenerates them; a teardown used to delete both.
#
# The host binaries, which are build output. KEEP_BIN=0 removes those too.
#
# Secrets are not on this list at all: they live in devnet/.env, outside .state, where a
# teardown cannot reach them.
tmp="$(mktemp -d)"
[ "${KEEP_BIN}" = "1" ] && [ -d "${BIN_DIR}" ] && mv "${BIN_DIR}" "${tmp}/bin"
for keep in coprocessor.toml gas-oracle.toml; do
  [ -f "${STATE_DIR}/${keep}" ] && mv "${STATE_DIR}/${keep}" "${tmp}/${keep}"
done
mkdir -p "${tmp}/pccs"
for f in "${OUT_DIR}"/pccs-*.json; do [ -f "$f" ] && cp "$f" "${tmp}/pccs/"; done
rm -rf "${STATE_DIR}"
mkdir -p "${STATE_DIR}"
[ -d "${tmp}/bin" ] && mv "${tmp}/bin" "${BIN_DIR}"
for keep in coprocessor.toml gas-oracle.toml; do
  [ -f "${tmp}/${keep}" ] && mv "${tmp}/${keep}" "${STATE_DIR}/${keep}"
done
mkdir -p "${OUT_DIR}"
for f in "${tmp}"/pccs/pccs-*.json; do [ -f "$f" ] && mv "$f" "${OUT_DIR}/"; done
rm -rf "${tmp}"

say "devnet is down"
