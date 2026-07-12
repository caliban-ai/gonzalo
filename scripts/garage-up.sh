#!/usr/bin/env bash
# Bring up a single-node Garage (S3) for the gonzalo HA soak (#52) and print the
# env exports the soak reads. Idempotent-ish: safe to re-run; reuses the volume.
#
#   eval "$(scripts/garage-up.sh)"      # start Garage + export the target
#   cargo test -p gonzalo-soak --test ha_soak   # run the bounded gate
#   scripts/garage-down.sh              # tear down
#
# All human-readable progress goes to stderr; only the `export` lines go to
# stdout, so `eval "$(...)"` picks up exactly the target env.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"
compose=(docker compose -f docker-compose.garage.yml)
gexec() { "${compose[@]}" exec -T garage /garage "$@"; }
log() { echo "garage-up: $*" >&2; }

# 1. Config (generated once; keep the secret stable across restarts).
if [ ! -f garage.toml ]; then
  log "generating garage.toml"
  rpc_secret="$(openssl rand -hex 32)"
  admin_token="$(openssl rand -hex 32)"
  cat > garage.toml <<EOF
metadata_dir = "/var/lib/garage/meta"
data_dir = "/var/lib/garage/data"
db_engine = "sqlite"
replication_factor = 1
# Bind all interfaces inside the container so the compose port mappings can reach
# the S3/admin APIs from the host. RPC stays a container-internal self-reference.
rpc_bind_addr = "[::]:3901"
rpc_public_addr = "127.0.0.1:3901"
rpc_secret = "${rpc_secret}"

[s3_api]
s3_region = "garage"
api_bind_addr = "[::]:3900"
root_domain = ".s3.garage"

[admin]
api_bind_addr = "[::]:3903"
admin_token = "${admin_token}"
EOF
fi

# 2. Start Garage and wait for the CLI to answer.
log "starting Garage container"
"${compose[@]}" up -d >&2
for _ in $(seq 1 60); do
  if gexec status >/dev/null 2>&1; then break; fi
  sleep 1
done
gexec status >/dev/null 2>&1 || { log "Garage did not come up"; "${compose[@]}" logs garage >&2; exit 1; }

# 3. Assign a storage layout (once). A node with a layout already applied is
#    left as-is.
if ! gexec layout show 2>/dev/null | grep -q 'zone'; then
  node="$(gexec node id -q | cut -d@ -f1)"
  log "assigning layout to node ${node}"
  gexec layout assign -z dc1 -c 1G "$node" >&2
  gexec layout apply --version 1 >&2
fi

# 4. Bucket + access key (idempotent).
bucket="${GONZALO_S3_TEST_BUCKET:-soak}"
gexec bucket create "$bucket" >/dev/null 2>&1 || true

if ! gexec key info --show-secret soak-key >/dev/null 2>&1; then
  log "creating access key soak-key"
  gexec key create soak-key >/dev/null 2>&1 || true
fi
keyinfo="$(gexec key info --show-secret soak-key)"
key_id="$(echo "$keyinfo" | sed -n 's/^Key ID: *//p' | tr -d '[:space:]')"
secret="$(echo "$keyinfo" | sed -n 's/^Secret key: *//p' | tr -d '[:space:]')"
gexec bucket allow --read --write --owner "$bucket" --key soak-key >/dev/null 2>&1 || true

[ -n "$key_id" ] && [ -n "$secret" ] || { log "failed to read access key"; echo "$keyinfo" >&2; exit 1; }

log "ready — bucket=${bucket} endpoint=http://127.0.0.1:3900"
# stdout: the target env for `eval`.
echo "export GONZALO_S3_TEST_ENDPOINT=http://127.0.0.1:3900"
echo "export GONZALO_S3_TEST_BUCKET=${bucket}"
echo "export GONZALO_S3_TEST_REGION=garage"
echo "export AWS_ACCESS_KEY_ID=${key_id}"
echo "export AWS_SECRET_ACCESS_KEY=${secret}"
