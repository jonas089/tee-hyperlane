#!/usr/bin/env bash
# Install the coprocessor and the bridge UI on a fresh Ubuntu host.
#
# Layout:
#   /opt/tee-hyperlane      binary, submit scripts, keys, route config
#   /opt/bridge-app         the built UI, served on :3000
#   /var/lib/tee-hyperlane  proof store, celestia keyring, SP1 artifact cache
#
# Idempotent: re-running it upgrades in place.
#
# No web server: the UI binary serves its own static files and proxies the API and Celestia's
# REST onto one origin, so nothing here competes for :80.
set -euo pipefail

SRC=${SRC:-/opt/tee-isms}
# The services run as this user. It needs no login and owns only its own state directory.
SERVICE_USER=${SERVICE_USER:-bridge}

if [ "$SERVICE_USER" = "bridge" ]; then
  id bridge >/dev/null 2>&1 || useradd --system --home /var/lib/tee-hyperlane --shell /usr/sbin/nologin bridge
fi

install -d -o "$SERVICE_USER" -g "$SERVICE_USER" /var/lib/tee-hyperlane/proofs /var/lib/tee-hyperlane/celhome
install -d /opt/tee-hyperlane/bin /opt/tee-hyperlane/deploy /opt/tee-hyperlane/keys /opt/tee-hyperlane/elf

install -m755 "$SRC/tee-hyperlane/target/release/tee-hyperlane" /opt/tee-hyperlane/bin/
install -m755 "$SRC/tee-hyperlane/target/release/gas-oracle" /opt/tee-hyperlane/bin/
install -m755 "$SRC/deploy/submit-evm.sh" "$SRC/deploy/submit-celestia.sh" /opt/tee-hyperlane/deploy/
install -m600 "$SRC/keys/SEPOLIA_PRIVATE_KEY.md" /opt/tee-hyperlane/keys/
install -m644 "$SRC/tee-circuit/elf/tee-state-transition" "$SRC/tee-circuit/elf/tee-state-membership" /opt/tee-hyperlane/elf/
chown -R "$SERVICE_USER":"$SERVICE_USER" /opt/tee-hyperlane/keys

# The submit scripts shell out to these; the service cannot see a user's home.
command -v cast >/dev/null || install -m755 "$HOME/.foundry/bin/cast" /usr/local/bin/cast

# The relayer's Celestia key. Same account that owns the warp tokens; it only pays gas.
if ! celestia-appd keys show bridge --home /var/lib/tee-hyperlane/celhome --keyring-backend test >/dev/null 2>&1; then
  celestia-appd keys import-hex bridge "$(tr -d ' \n\r' < "$SRC/keys/CELESTIA_MOCHA_PRIVATE_KEY.md")" \
    --home /var/lib/tee-hyperlane/celhome --keyring-backend test
fi

# Services are restarted, not just enabled: `enable --now` leaves an already-running unit on
# the old binary, which is how an upgrade silently does nothing.
chown -R "$SERVICE_USER":"$SERVICE_USER" /var/lib/tee-hyperlane

install -m644 "$SRC/deploy/server/coprocessor.toml" /opt/tee-hyperlane/coprocessor.toml
install -m644 "$SRC/deploy/server/gas-oracle.toml" /opt/tee-hyperlane/gas-oracle.toml

rm -rf /opt/bridge-app
install -d /opt/bridge-app
cp -r "$SRC/bridge-app/dist/." /opt/bridge-app/

install -m644 "$SRC/deploy/server/tee-hyperlane.service" \
  "$SRC/deploy/server/tee-hyperlane-api.service" \
  "$SRC/deploy/server/gas-oracle.service" \
  "$SRC/deploy/server/bridge-ui.service" /etc/systemd/system/
systemctl daemon-reload
UNITS="tee-hyperlane-api tee-hyperlane gas-oracle bridge-ui"
systemctl enable $UNITS
systemctl restart $UNITS

systemctl --no-pager --lines=0 status $UNITS || true
