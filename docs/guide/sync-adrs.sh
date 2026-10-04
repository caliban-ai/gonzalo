#!/usr/bin/env bash
# Copy docs/adr/*.md into the mdBook guide and regenerate the ADR SUMMARY block.
# Run from the repo root (the docs workflow runs it before `mdbook build`).
# Portable across BSD (macOS) and GNU awk.
set -euo pipefail

ADR_SRC="docs/adr"
GUIDE_SRC="docs/guide/src"
DEST="$GUIDE_SRC/adr"
SUMMARY="$GUIDE_SRC/SUMMARY.md"
MARKER="<!-- adrs -->"

# Files in docs/adr that are not themselves ADRs.
is_adr() {
  case "$(basename "$1")" in
    index.md | README.md | template.md) return 1 ;;
    *) return 0 ;;
  esac
}

mkdir -p "$DEST"

# Copy the ADRs, rewriting the one link class that cannot survive the move.
# An ADR cites its design spec as `../superpowers/specs/<name>.md`, which is
# correct in `docs/adr/` on GitHub. The specs and plans are build notes and are
# deliberately NOT ingested into the book, so inside `src/adr/` that path
# resolves to nothing and every such link 404s on the published guide. Point
# them at the repository instead. The ADR log itself is append-only and stays
# untouched (ADR 0001); this rewrite lives here, on the way in.
# BSD (macOS) and GNU sed agree on this subset: plain `s|…|…|g`, no -i.
REPO_BLOB="https://github.com/caliban-ai/gonzalo/blob/main/docs"
for f in "$ADR_SRC"/*.md; do
  sed -e "s|](../superpowers/|]($REPO_BLOB/superpowers/|g" \
      "$f" > "$DEST/$(basename "$f")"
done

# Build an ADR index page from the file titles (first markdown H1 of each file).
{
  echo "# Architecture Decision Records"
  echo
  for f in "$DEST"/*.md; do
    is_adr "$f" || continue
    base="$(basename "$f")"
    title="$(grep -m1 '^# ' "$f" | sed 's/^# //')"
    echo "- [${title:-$base}](./${base})"
  done
} > "$DEST/index.md"

# Build the nested SUMMARY entries (newest mdBook needs every page listed).
entries=""
for f in "$DEST"/*.md; do
  is_adr "$f" || continue
  base="$(basename "$f")"
  title="$(grep -m1 '^# ' "$f" | sed 's/^# //')"
  entries+="  - [${title:-$base}](./adr/${base})"$'\n'
done

# Regenerate everything after the marker: keep the file up to and including the
# marker line, then append the fresh entries. Uses sed (not a multi-line awk -v),
# which is portable across BSD/macOS and GNU awk.
grep -qF -- "$MARKER" "$SUMMARY" || {
  echo "error: ADR marker '$MARKER' not found in $SUMMARY" >&2
  exit 1
}
tmp="$SUMMARY.tmp"
sed "/$MARKER/q" "$SUMMARY" > "$tmp"
printf '%s' "$entries" >> "$tmp"
mv "$tmp" "$SUMMARY"
