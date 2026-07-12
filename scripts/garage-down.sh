#!/usr/bin/env bash
# Tear down the single-node Garage brought up by garage-up.sh (#52).
# Pass --purge to also drop the data/metadata volumes and generated config.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"
compose=(docker compose -f docker-compose.garage.yml)
if [ "${1:-}" = "--purge" ]; then
  "${compose[@]}" down -v
  rm -f garage.toml
else
  "${compose[@]}" down
fi
