#!/usr/bin/env bash
# Tear down the single-node RustFS brought up by rustfs-up.sh (#52).
# Pass --purge to also drop the data volume.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"
compose=(docker compose -f docker-compose.rustfs.yml)
if [ "${1:-}" = "--purge" ]; then
  "${compose[@]}" down -v
else
  "${compose[@]}" down
fi
