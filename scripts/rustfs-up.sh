#!/usr/bin/env bash
# Bring up a single-node RustFS (S3) for the gonzalo HA soak (#52) and print the
# env exports the soak reads. RustFS provides atomic If-Match conditional writes
# (soak-verified) and is Rust + Apache-2.0 — see the design doc for why Garage and
# MinIO were rejected.
#
#   eval "$(scripts/rustfs-up.sh)"
#   cargo test -p gonzalo-soak --test ha_soak
#   scripts/rustfs-down.sh
#
# Only the `export` lines go to stdout (for `eval`); progress goes to stderr.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"
compose=(docker compose -f docker-compose.rustfs.yml)
bucket="${GONZALO_S3_TEST_BUCKET:-soak}"
log() { echo "rustfs-up: $*" >&2; }

log "starting RustFS + bucket creator"
"${compose[@]}" up -d >&2

# Wait for the RustFS S3 endpoint to accept connections (any HTTP status, incl.
# 403 for unauthenticated, means it's up; 000 means not listening yet).
ready=""
for _ in $(seq 1 60); do
  code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:9010/" || true)"
  if [ -n "$code" ] && [ "$code" != "000" ]; then ready=1; break; fi
  sleep 1
done
[ -n "$ready" ] || { log "RustFS did not come up"; "${compose[@]}" logs rustfs >&2; exit 1; }

# Wait for the one-shot bucket creator to finish.
for _ in $(seq 1 60); do
  if "${compose[@]}" logs createbucket 2>/dev/null | grep -q "bucket ready"; then break; fi
  sleep 1
done
"${compose[@]}" logs createbucket 2>/dev/null | grep -q "bucket ready" \
  || { log "bucket not created"; "${compose[@]}" logs createbucket >&2; exit 1; }

log "ready — bucket=${bucket} endpoint=http://127.0.0.1:9010"
echo "export GONZALO_S3_TEST_ENDPOINT=http://127.0.0.1:9010"
echo "export GONZALO_S3_TEST_BUCKET=${bucket}"
echo "export GONZALO_S3_TEST_REGION=us-east-1"
echo "export AWS_ACCESS_KEY_ID=rustfsadmin"
echo "export AWS_SECRET_ACCESS_KEY=rustfsadmin"
