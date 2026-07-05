#!/usr/bin/env bash
# Copy the root CHANGELOG.md into the mdBook guide as a single page.
# Run from the repo root (the docs workflow runs it before `mdbook build`).
# The copied docs/guide/src/changelog.md is gitignored; CHANGELOG.md is the source.
set -euo pipefail

SRC="CHANGELOG.md"
DEST="docs/guide/src/changelog.md"

if [[ ! -f "$SRC" ]]; then
  echo "error: $SRC not found (run from the repo root)" >&2
  exit 1
fi

cp "$SRC" "$DEST"
