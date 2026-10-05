# Shared helpers for the devnet scripts. Sourced, never executed.
# shellcheck shell=bash

set -Eeuo pipefail

# No script stops without saying where. `set -e` alone exits on the first unchecked failure
# and prints nothing, which is how 85-celestia-isms.sh once stopped halfway with no clue why.
# The trap names the script, the line and the command; -E carries it into functions and
# command substitutions.
trap 'printf "\033[1;31merror\033[0m %s:%s: \`%s\` failed (exit %s)\n" \
  "${BASH_SOURCE[0]##*/}" "${LINENO}" "${BASH_COMMAND}" "$?" >&2' ERR

# Work a script had to leave undone. Each item is warned about as it happens, and the script
# then exits 3 with the list instead of 0, so a partial run is never mistaken for a finished
# one. Re-running the script picks up where it stopped: every step here is idempotent.
INCOMPLETE=()
incomplete() {
  printf '\033[1;33m warn\033[0m %s\n' "$*" >&2
  INCOMPLETE+=("$*")
}
report_incomplete() {
  local rc=$?
  if [ "${rc}" -eq 0 ] && [ "${#INCOMPLETE[@]}" -gt 0 ]; then
    printf '\n\033[1;33mincomplete\033[0m %s: %s item(s) left undone\n' "${0##*/}" "${#INCOMPLETE[@]}" >&2
    printf '  - %s\n' "${INCOMPLETE[@]}" >&2
    printf 'fix the cause and run it again; finished steps are skipped\n' >&2
    exit 3
  fi
}
trap report_incomplete EXIT

DEVNET_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_DIR="$(cd "${DEVNET_DIR}/.." && pwd)"
STATE_DIR="${STATE_DIR:-${DEVNET_DIR}/.state}"
OUT_DIR="${STATE_DIR}/out"

# Every secret this devnet needs lives in one file, devnet/.env, copied from devnet/.env.example.
# It sits outside .state deliberately: `make stop` deletes that directory, and a teardown has
# no business eating the phrase behind an imported wallet.
ENV_FILE="${ENV_FILE:-${DEVNET_DIR}/.env}"
if [ -f "${ENV_FILE}" ]; then
  # `set -a` so every assignment in the file is exported without the file needing to say so.
  # That keeps the syntax the same one systemd accepts, since the relayer unit reads this
  # very file as its EnvironmentFile.
  set -a; . "${ENV_FILE}"; set +a
elif [ -z "${EVM_PRIVATE_KEY:-}" ]; then
  # Not fatal, because bringing the chain up needs none of it. Said once, here, rather than
  # left for whichever step first wants a key to fail on an unset variable.
  printf '\033[1;33m warn\033[0m no %s. Copy %s/.env.example to it and fill it in.\n' \
    "${ENV_FILE}" "${DEVNET_DIR}" >&2
fi

CELESTIA_APP_DIR="${CELESTIA_APP_DIR:-${REPO_DIR}/../celestia-app-local}"
CELESTIA_IMAGE="${CELESTIA_IMAGE:-celestia-app-teeism:local}"
CELESTIA_CONTAINER="${CELESTIA_CONTAINER:-teeism-celestia}"
CHAINID="${CHAINID:-teeism-local}"
CELESTIA_RPC="${CELESTIA_RPC:-http://localhost:26657}"
CELESTIA_API="${CELESTIA_API:-http://localhost:1317}"

# The devnet chain's Hyperlane domain. Deliberately not Mocha's: the EVM side
# enrolls routers by domain, and reusing a live domain would let a devnet router
# and a testnet router be confused for one another.
CELESTIA_DOMAIN="${CELESTIA_DOMAIN:-1297040299}"

SEPOLIA_DOMAIN=11155111
BASE_SEPOLIA_DOMAIN=84532
ARBITRUM_SEPOLIA_DOMAIN=421614
# Eden, the evolve-stack chain. 0xdeadbfee.
EDEN_DOMAIN=3735928814

TX_FEES="${TX_FEES:-200000utia}"
TX_GAS="${TX_GAS:-900000}"

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m warn\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror\033[0m %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"; }

# set_env <key> <value> - record a generated value in devnet/.env, replacing the line if it
# is already there. Anything the scripts mint that must outlive a teardown goes back into the
# one file the operator already knows about, rather than getting a second home under .state.
set_env() {
  local key="$1" val="$2" tmp
  if [ ! -f "${ENV_FILE}" ]; then
    printf '# Written by the devnet scripts. See devnet/.env.example.\n' > "${ENV_FILE}"
    chmod 600 "${ENV_FILE}"
  fi
  # A temp file and a copy, never `sed -i`: its in-place spelling differs between GNU and
  # BSD. This file may hold the only copy of a seed phrase, so that is not a difference to
  # discover here. The copy rather than a move preserves the mode already on the file.
  tmp="$(mktemp)"
  grep -v "^${key}=" "${ENV_FILE}" > "${tmp}" || true
  printf '%s="%s"\n' "${key}" "${val}" >> "${tmp}"
  cat "${tmp}" > "${ENV_FILE}"
  rm -f "${tmp}"
}

BIN_DIR="${BIN_DIR:-${STATE_DIR}/bin}"
APPD="${APPD:-${BIN_DIR}/celestia-appd}"
COLLATERAL_BIN="${COLLATERAL_BIN:-${BIN_DIR}/teeism-collateral}"
CELHOME="${CELHOME:-${STATE_DIR}/celestia}"

# The mnemonic the genesis accounts are derived from. Generated once and kept across
# teardowns, so the address a wallet imported still holds funds after the chain is rebuilt.
# Without this every genesis mints fresh random keys and the imported wallet goes empty.
ensure_mnemonic() {
  if [ -z "${CELESTIA_MNEMONIC:-}" ]; then
    CELESTIA_MNEMONIC="$("${APPD}" keys mnemonic 2>/dev/null | tr -d '\r' | head -1)"
    [ -n "${CELESTIA_MNEMONIC}" ] || die "could not generate a mnemonic"
    set_env CELESTIA_MNEMONIC "${CELESTIA_MNEMONIC}"
    say "generated a genesis mnemonic and wrote it to ${ENV_FILE}"
  fi
  DEVNET_MNEMONIC="${CELESTIA_MNEMONIC}"
  export DEVNET_MNEMONIC CELESTIA_MNEMONIC
}

# The key that pays for EVM deployments, from devnet/.env and nowhere else. Accepted with or
# without the 0x, because systemd reads this same file and needs the prefix, while a pasted
# key often does not have one.
if [ -n "${EVM_PRIVATE_KEY:-}" ]; then
  case "${EVM_PRIVATE_KEY}" in 0x*) ;; *) EVM_PRIVATE_KEY="0x${EVM_PRIVATE_KEY}" ;; esac
  export EVM_PRIVATE_KEY
fi

# The chain's home directory is a bind mount, so a host-side binary shares the container's
# keyring and can sign without shelling into it. That keeps file paths host paths, which
# matters because the submit step hands the node a file it just wrote.
appd() { "${APPD}" "$@" --home "${CELHOME}"; }

# Build the host-side binaries if they are missing, so pruning the build tree is recoverable.
ensure_binaries() {
  mkdir -p "${BIN_DIR}"
  [ -d "${CELESTIA_APP_DIR}" ] || die "no celestia-app checkout at ${CELESTIA_APP_DIR}; set CELESTIA_APP_DIR"
  if [ ! -x "${APPD}" ]; then
    say "building celestia-appd for the host"
    (cd "${CELESTIA_APP_DIR}" && go build -o "${APPD}" ./cmd/celestia-appd)
  fi
  if [ ! -x "${COLLATERAL_BIN}" ]; then
    say "building teeism-collateral"
    (cd "${CELESTIA_APP_DIR}" && go build -o "${COLLATERAL_BIN}" ./x/teeism/cmd/teeism-collateral)
  fi
}

# addr <key> - the bech32 address of a devnet key.
addr() { appd keys show "$1" -a --keyring-backend test; }

# Queries need a node, not a home, but passing both is harmless and keeps one helper.
q() { appd query "$@" --node "${CELESTIA_RPC}" -o json; }

# tx <key> <args...> - broadcast, wait for inclusion, and print the tx result as
# JSON. Fails loudly on a non-zero code rather than leaving a later step to
# discover the transaction never landed.
tx() {
  local key="$1"; shift
  local raw hash
  raw="$(appd tx "$@" \
    --from "${key}" --keyring-backend test --chain-id "${CHAINID}" \
    --node "${CELESTIA_RPC}" \
    --fees "${TX_FEES}" --gas "${TX_GAS}" \
    --broadcast-mode sync --yes --output json 2>&1)" || {
      printf '%s\n' "${raw}" >&2
      die "broadcast failed"
    }

  # The node echoes CheckTx first. A non-zero code here means it never entered a
  # block, so reporting the hash would be misleading.
  local code
  code="$(printf '%s' "${raw}" | jq -r 'select(.code != null) | .code' 2>/dev/null | head -1)"
  if [ -n "${code}" ] && [ "${code}" != "0" ]; then
    printf '%s\n' "${raw}" >&2
    die "transaction rejected at CheckTx with code ${code}"
  fi

  hash="$(printf '%s' "${raw}" | jq -r 'select(.txhash != null) | .txhash' 2>/dev/null | head -1)"
  [ -n "${hash}" ] || { printf '%s\n' "${raw}" >&2; die "no tx hash in response"; }

  wait_for_tx "${hash}"
}

# wait_for_tx <hash> - poll until the transaction is in a block, then print it.
wait_for_tx() {
  local hash="$1" i result code
  for i in $(seq 1 60); do
    if result="$(appd query tx "${hash}" --node "${CELESTIA_RPC}" --output json 2>/dev/null)"; then
      code="$(printf '%s' "${result}" | jq -r '.code // 0')"
      if [ "${code}" != "0" ]; then
        printf '%s\n' "$(printf '%s' "${result}" | jq -r '.raw_log // .rawLog // "no log"')" >&2
        die "transaction ${hash} failed on chain with code ${code}"
      fi
      printf '%s' "${result}"
      return 0
    fi
    sleep 1
  done
  die "timed out waiting for ${hash}"
}

# ev <tx-json> <event-type> <attribute> - read one attribute out of a tx result.
ev() {
  printf '%s' "$1" | jq -r --arg t "$2" --arg k "$3" '
    [ .events[]? | select(.type == $t) | .attributes[]? | select(.key == $k) | .value ]
    | first // empty
  ' | tr -d '"'
}

# save <name> <value> - record a deployed address so later steps and the relayer
# can read it back without re-parsing transaction logs.
save() {
  mkdir -p "${OUT_DIR}"
  printf '%s' "$2" > "${OUT_DIR}/$1"
  printf '  %-28s %s\n' "$1" "$2"
}

# load <name> - read back a saved value, failing if the step that writes it never ran.
load() {
  local f="${OUT_DIR}/$1"
  [ -f "${f}" ] || die "missing ${1}; run 'make init' first"
  cat "${f}"
}

has() { [ -f "${OUT_DIR}/$1" ]; }

# pad32 <0x-address> - left-pad a 20-byte EVM address to the 32 bytes a Hyperlane message
# carries. Lowercased, because the padded form is compared as bytes and a checksummed address
# and its lowercase spelling are the same address but different strings.
pad32() {
  printf '0x000000000000000000000000%s' \
    "$(printf '%s' "$1" | sed 's/^0x//' | tr 'A-F' 'a-f')"
}

# wait_for_chain - block until the node is answering and producing blocks.
wait_for_chain() {
  local i height
  for i in $(seq 1 90); do
    height="$(curl -s -m 2 "${CELESTIA_RPC}/status" 2>/dev/null \
      | jq -r '.result.sync_info.latest_block_height // empty' 2>/dev/null || true)"
    if [ -n "${height}" ] && [ "${height}" -gt 0 ] 2>/dev/null; then
      say "celestia is at height ${height}"
      return 0
    fi
    sleep 1
  done
  die "celestia did not start; check 'docker logs ${CELESTIA_CONTAINER}'"
}

# ---------------------------------------------------------------- enclave images
#
# One image per origin chain. By default all of them run in one CVM, deploy/docker-compose.all.yml;
# `30-enclave-up.sh --<chain>` runs one alone in its own CVM, deploy/docker-compose.<chain>.yml.
# Each chain answers on the same port either way, so its URL only differs by app id.
ENCLAVE_CHAINS="celestia ethereum base arbitrum eden"

enclave_port() { # <chain>
  case "$1" in
    celestia) echo 8080 ;; ethereum) echo 8081 ;; base) echo 8082 ;;
    arbitrum) echo 8083 ;; eden) echo 8084 ;; *) return 1 ;;
  esac
}

# The enclave that attests an origin, by the coprocessor's chain name.
enclave_for() { # <origin>
  case "$1" in sepolia) echo ethereum ;; *) echo "$1" ;; esac
}

compose_file() { echo "${REPO_DIR}/deploy/docker-compose.$1.yml"; } # <chain|all>

# The shared CVM's compose: every chain's service exactly as its own file pins it, so the
# digests live in one place and this file can be regenerated and compared.
compose_all() {
  cat <<'YAML'
# The measured configuration for the shared CVM: every origin's enclave, each on its own port.
# Generated by devnet/scripts/25-images.sh from deploy/docker-compose.<chain>.yml; do not edit.
#
# This file's hash becomes `compose-hash` in RTMR3, so every ISM attested from this CVM pins
# all of it: changing any one image here moves the identity of every chain in it. A chain
# deployed alone (`30-enclave-up.sh --<chain>`) measures only its own file instead.
services:
YAML
  local chain
  for chain in ${ENCLAVE_CHAINS}; do
    sed -n '/^services:/,$p' "$(compose_file "${chain}")" | tail -n +2
  done
}

# deploy/images.lock records, per chain, the Nix store path an image was built from and the
# registry digest it was pushed as: `<chain> <outPath> <digest>`. 25-images.sh writes it;
# 30-enclave-up.sh refuses to deploy unless the current source still evaluates to that path.
IMAGES_LOCK="${REPO_DIR}/deploy/images.lock"

# The store path the current source would build for a chain. Evaluates, never builds.
image_out_path() { nix eval --raw "${REPO_DIR}#image-$1.outPath"; }

# The digest a chain's compose file pins.
pinned_digest() { grep -o 'tee-node@sha256:[0-9a-f]*' "$(compose_file "$1")" | cut -d@ -f2; }

# locked <chain> <out|digest> - that chain's recorded value, or nothing.
locked() {
  local col=2; [ "$2" = digest ] && col=3
  awk -v f="$1" -v c="${col}" '$1 == f { print $c }' "${IMAGES_LOCK}" 2>/dev/null || true
}

# ---------------------------------------------------------------- the coprocessor config
#
# One generator for `${STATE_DIR}/coprocessor.toml`, called before anything reads it: the ISM
# scripts need the chain tables to produce genesis states, and `make start` needs the routes.
# Every chain is always written; a route only once its ISM exists. Endpoint defaults match the
# live deployment; override any of them in devnet/.env.
COPROCESSOR_CONFIG="${STATE_DIR}/coprocessor.toml"
COPROCESSOR_BIN="${REPO_DIR}/tee-hyperlane/target/release/tee-hyperlane"

write_config() {
  local sepolia_rpc="${SEPOLIA_RPC:-https://rpc.sepolia.ethpandaops.io}"
  # The archives for Base and Arbitrum: BASE_ARCHIVE / ARBITRUM_ARCHIVE as urls, else Alchemy
  # with ALCHEMY_KEY from devnet/.env (ALCHEMY_BASE_KEY on older hosts, or the key file they
  # keep in .state). Only these two `rpc` fields ever get it.
  local key="${ALCHEMY_KEY:-${ALCHEMY_BASE_KEY:-}}"
  if [ -z "${key}" ] && [ -f "${STATE_DIR}/alchemy-base-key" ]; then
    key="$(cat "${STATE_DIR}/alchemy-base-key")"
  fi
  local base_archive="${BASE_ARCHIVE:-${key:+https://base-sepolia.g.alchemy.com/v2/${key}}}"
  local arbitrum_archive="${ARBITRUM_ARCHIVE:-${key:+https://arb-sepolia.g.alchemy.com/v2/${key}}}"
  # Always built, not only when missing: a binary left over from older source would take a
  # config it does not understand, or lack the `genesis` the ISM scripts call. Up to date, this
  # is a no-op.
  (cd "${REPO_DIR}/tee-hyperlane" && cargo build --quiet --release -p tee-coprocessor)

  # Our warp routers on a chain, as a TOML list. An asset that was not deployed is left out
  # rather than written empty: an empty id would match nothing and quietly widen the filter.
  routers() {
    local out="" key
    for key in "$@"; do has "${key}" && out="${out}${out:+, }\"$(load "${key}")\""; done
    printf '[%s]' "${out}"
  }
  route() { # <name> <from> <to> <ism key> <router keys...>
    local name="$1" from="$2" to="$3" ism="$4" enclave
    shift 4
    enclave="$(enclave_for "${from}")"
    has "${ism}" && has "enclave-url-${enclave}" || return 0
    printf '\n[[routes]]\nname = "%s"\nfrom = "%s"\nto = "%s"\nenclave = "%s"\nism = "%s"\nrouters = %s\n' \
      "${name}" "${from}" "${to}" "$(load "enclave-url-${enclave}")" "$(load "${ism}")" "$(routers "$@")"
  }

  {
    cat <<TOML
# Generated by devnet/scripts/lib.sh write_config. Regenerated on every run from what is
# deployed, so editing it by hand does not survive. See deploy/coprocessor.toml.example.
tick_secs = ${TICK_SECS:-2}
proof_dir = "${STATE_DIR}/proofs"
api_listen = "${API_LISTEN:-0.0.0.0:3001}"

[faucet]
chain = "celestia"

[chains.celestia]
kind = "celestia"
domain = ${CELESTIA_DOMAIN}
rpc = "${CELESTIA_RPC}"
mailbox = "$(has mailbox-id && load mailbox-id)"
merkle_tree_hook = "$(has merkle-hook-id && load merkle-hook-id)"
lag = 1
chain_id = "${CHAINID}"
home = "${CELHOME}"

[chains.mocha]
kind = "celestia"
domain = 1297040200
rpc = "${MOCHA_RPC:-https://rpc.celestia-mocha.com}"

[chains.sepolia]
kind = "ethereum"
domain = ${SEPOLIA_DOMAIN}
rpc = "${sepolia_rpc}"
beacon_rpc = "${SEPOLIA_BEACON:-https://ethereum-sepolia-beacon-api.publicnode.com}"
mailbox = "${SEPOLIA_MAILBOX:-0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766}"
merkle_tree_hook = "${SEPOLIA_HOOK:-0x4917a9746A7B6E0A57159cCb7F5a6744247f2d0d}"

[chains.arbitrum]
kind = "arbitrum"
domain = ${ARBITRUM_SEPOLIA_DOMAIN}
rpc = "${arbitrum_archive:-https://api.zan.top/arb-sepolia}"
feed = "${ARBITRUM_FEED:-wss://sepolia-rollup.arbitrum.io/feed}"
logs_rpc = "${ARBITRUM_LOGS:-https://sepolia-rollup.arbitrum.io/rpc}"
send_rpc = "${ARBITRUM_RPC:-https://sepolia-rollup.arbitrum.io/rpc}"
mailbox = "${ARBITRUM_MAILBOX:-0x598facE78a4302f11E3de0bee1894Da0b2Cb71F8}"
merkle_tree_hook = "${ARBITRUM_HOOK:-0xAD34A66Bf6dB18E858F6B686557075568c6E031C}"

[chains.base]
kind = "base"
domain = ${BASE_SEPOLIA_DOMAIN}
rpc = "${base_archive:-https://sepolia.base.org}"
p2p_port = ${BASE_P2P_PORT:-9222}
logs_rpc = "${BASE_RPC:-https://sepolia.base.org}"
send_rpc = "${BASE_RPC:-https://sepolia.base.org}"
mailbox = "${BASE_MAILBOX:-0x6966b0E55883d49BFB24539356a2f8A673E02039}"
merkle_tree_hook = "${BASE_HOOK:-0x86fb9F1c124fB20ff130C41a79a432F770f67AFD}"

[chains.eden]
kind = "eden"
domain = ${EDEN_DOMAIN}
celestia = "mocha"
da_rpc = "${EDEN_DA_RPC:-http://localhost:26658}"
rpc = "${EDEN_ARCHIVE:-https://ev-reth-eden-testnet.binarybuilders.services:8545/}"
logs_rpc = "${EDEN_RPC:-https://rpc.testnet.eden.gateway.fm/}"
send_rpc = "${EDEN_RPC:-https://rpc.testnet.eden.gateway.fm/}"
mailbox = "${EDEN_MAILBOX:-0x1D32350f3440BEa7f7E450Aa085f63E0d7E38729}"
merkle_tree_hook = "${EDEN_HOOK:-0xCfBE7016D123d52A7Db4fc7D087cCb5421dbF8db}"
TOML
    local chain
    for chain in sepolia arbitrum base eden; do
      route "celestia-to-${chain}" celestia "${chain}" "ism-${chain}" "${chain}-router" "${chain}-usdc-router"
    done
    for chain in sepolia arbitrum base eden; do
      route "${chain}-to-celestia" "${chain}" celestia "ism-celestia-${chain}" celestia-token-id celestia-usdc-token-id
    done
  } > "${COPROCESSOR_CONFIG}"
}
