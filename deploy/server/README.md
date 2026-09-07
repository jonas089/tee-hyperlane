# Server deployment

One machine runs the coprocessor, the attestation API and the UI. The two enclaves stay on
Phala; nothing here holds a TEE.

```sh
useradd -r -s /usr/sbin/nologin bridge
mkdir -p /opt/tee-hyperlane/bin /opt/bridge-app /var/lib/tee-hyperlane
chown -R bridge:bridge /var/lib/tee-hyperlane

# Binary and config
install -m755 target/release/tee-hyperlane /opt/tee-hyperlane/bin/
install -m640 -o bridge .env               /opt/tee-hyperlane/.env
install -m644 deploy/coprocessor.toml.example /opt/tee-hyperlane/coprocessor.toml

# UI
rsync -a bridge-app/dist/ /opt/bridge-app/

# Submit scripts, which the relayer shells out to for signing
cp -r deploy /opt/tee-hyperlane/deploy

# Services
cp deploy/server/*.service /etc/systemd/system/
cp deploy/server/nginx.conf /etc/nginx/sites-available/bridge
ln -sf /etc/nginx/sites-available/bridge /etc/nginx/sites-enabled/bridge
systemctl daemon-reload
systemctl enable --now tee-hyperlane tee-hyperlane-api nginx
```

Sizing: proving is two Groth16 wraps per batch, about 280 s each on a modern core and around
16 GB peak. Give it 8+ real cores and 32 GB, or batches queue behind each other.

`cast` (foundry) and `celestia-appd` must be on the service's PATH: the relayer shells out to
them to sign, rather than reimplementing two transaction formats.

`.env` holds the relayer's signing keys. It is the only secret on the box, and it is only a
relayer key: it pays gas and can stall the bridge, but it cannot make either chain accept a
message the enclave did not attest.
