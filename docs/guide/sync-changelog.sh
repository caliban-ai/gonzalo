#!/usr/bin/env bash
# Copy the root CHANGELOG.md into the mdBook guide as a single page.
# Run from the repo root (the docs workflow runs it before `mdbook build`).
# The copied docs/guide/src/changelog.md is gitignored; CHANGELOG.md is the source.
#
# Links are rewritten on the way in. CHANGELOG.md is read in two places and its
# links must work in both: on GitHub it sits at the repo root, so an ADR is
# `docs/adr/NNNN-*.md`, while in the book it is `src/changelog.md` beside the
# ingested `src/adr/`, where that same path resolves to nothing. The source keeps
# the repo-relative form and this script maps it to the book's layout, so a
# changelog entry never has to choose which reader to break.
set -euo pipefail

SRC="CHANGELOG.md"
DEST="docs/guide/src/changelog.md"

if [[ ! -f "$SRC" ]]; then
  echo "error: $SRC not found (run from the repo root)" >&2
  exit 1
fi

# BSD (macOS) and GNU sed agree on this subset: plain `s|…|…|g`, no -i.
sed -e 's|](docs/adr/|](./adr/|g' \
    -e 's|](docs/guide/src/|](./|g' \
    "$SRC" > "$DEST"
