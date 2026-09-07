#!/usr/bin/env bash
# Install the coprocessor and the bridge UI on a fresh Ubuntu host.
#
# Layout:
#   /opt/tee-hyperlane      binary, submit scripts, keys, route config
#   /opt/bridge-app         the built UI, served by nginx on :3000
#   /var/lib/tee-hyperlane  proof store, celestia keyring, SP1 artifact cache
#
# Idempotent: re-running it upgrades in place.
set -euo pipefail

SRC=${SRC:-/opt/tee-isms}

id bridge >/dev/null 2>&1 || useradd --system --home /var/lib/tee-hyperlane --shell /usr/sbin/nologin bridge

install -d -o bridge -g bridge /var/lib/tee-hyperlane/proofs /var/lib/tee-hyperlane/celhome
install -d /opt/tee-hyperlane/bin /opt/tee-hyperlane/deploy /opt/tee-hyperlane/keys /opt/tee-hyperlane/elf

install -m755 "$SRC/tee-hyperlane/target/release/tee-hyperlane" /opt/tee-hyperlane/bin/
install -m755 "$SRC/deploy/submit-evm.sh" "$SRC/deploy/submit-celestia.sh" /opt/tee-hyperlane/deploy/
install -m600 "$SRC/keys/SEPOLIA_PRIVATE_KEY.md" /opt/tee-hyperlane/keys/
install -m644 "$SRC/tee-circuit/elf/tee-state-transition" "$SRC/tee-circuit/elf/tee-state-membership" /opt/tee-hyperlane/elf/
chown -R bridge:bridge /opt/tee-hyperlane/keys

# The submit scripts shell out to these; the service cannot see /root.
install -m755 /root/.foundry/bin/cast /usr/local/bin/cast

# The relayer's Celestia key. Same account that owns the warp tokens; it only pays gas.
if ! celestia-appd keys show bridge --home /var/lib/tee-hyperlane/celhome --keyring-backend test >/dev/null 2>&1; then
  celestia-appd keys import-hex bridge "$(tr -d ' \n\r' < "$SRC/keys/CELESTIA_MOCHA_PRIVATE_KEY.md")" \
    --home /var/lib/tee-hyperlane/celhome --keyring-backend test
fi
chown -R bridge:bridge /var/lib/tee-hyperlane

install -m644 "$SRC/deploy/server/coprocessor.toml" /opt/tee-hyperlane/coprocessor.toml

rm -rf /opt/bridge-app
install -d /opt/bridge-app
cp -r "$SRC/bridge-app/dist/." /opt/bridge-app/

install -m644 "$SRC/deploy/server/nginx.conf" /etc/nginx/sites-available/bridge-app
ln -sf /etc/nginx/sites-available/bridge-app /etc/nginx/sites-enabled/bridge-app
rm -f /etc/nginx/sites-enabled/default
nginx -t
systemctl reload nginx

install -m644 "$SRC/deploy/server/tee-hyperlane.service" "$SRC/deploy/server/tee-hyperlane-api.service" /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now tee-hyperlane-api.service
systemctl enable --now tee-hyperlane.service

systemctl --no-pager --lines=0 status tee-hyperlane-api tee-hyperlane || true
