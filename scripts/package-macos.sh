#!/usr/bin/env bash
#
# Build and package the gonzalo Apple Silicon release archive (gonzalo#229).
#
# Why this exists: the release-binaries workflow runs on `v*` tags only, so it
# gets no rehearsal on a pull request — a mistake in it surfaces when a release
# is already tagged and half-published. Keeping the whole build → sign →
# smoke-test → archive chain in a script that CI merely *calls* means the exact
# code that runs on the runner can be run first on a developer's Mac, which is
# the same platform. Rehearsal by construction rather than by CI minutes.
#
# What ships: the three binaries that must agree on EXTRACTION_VERSION —
# `gonzalo` (CLI), `gonzalo-mcp` (MCP server) and `gonzalo-parse-worker` (the
# crash-isolated parser). Installed one at a time from crates.io they can drift
# apart (#212, #228); one archive with one of each removes that by construction.
# Not `gonzalod` (the container image is its channel) and not `gonzalo-soak`.
#
# Usage:
#   scripts/package-macos.sh                # version read from the workspace
#   scripts/package-macos.sh v0.5.0         # assert the tag matches, then build
#   SKIP_BUILD=1 scripts/package-macos.sh   # repackage an existing target/ build
#
# Output: dist/gonzalo-vX.Y.Z-aarch64-apple-darwin.tar.gz (+ .sha256)
#
set -euo pipefail

TARGET="aarch64-apple-darwin"
BINS=(gonzalo gonzalo-mcp gonzalo-parse-worker)
SKIP_BUILD="${SKIP_BUILD:-0}"

# BSD tar sidecars an AppleDouble `._file` for every extended attribute, and a
# code signature is one — without this the archive carries three junk files.
export COPYFILE_DISABLE=1

cd "$(dirname "$0")/.."

# --- guards ------------------------------------------------------------------

# `cargo build --target aarch64-apple-darwin` would happily cross-compile from
# an x86_64 host, producing binaries this script cannot smoke-test. Refuse.
arch="$(uname -m)"
if [ "$arch" != "arm64" ]; then
  echo "error: this packages Apple Silicon binaries and smoke-tests them;" >&2
  echo "       host arch is '$arch', expected 'arm64'." >&2
  exit 1
fi

# The workspace version is carried by every member; read it off gonzalo-core, a
# leaf lib present in every configuration (same source publish.yml uses).
version="$(cargo metadata --no-deps --format-version 1 \
           | jq -r '.packages[] | select(.name=="gonzalo-core") | .version')"

if [ "$#" -ge 1 ]; then
  want="${1#v}"
  if [ "$want" != "$version" ]; then
    echo "error: tag v$want does not match workspace version $version" >&2
    exit 1
  fi
fi

tag="v$version"
pkg="gonzalo-$tag-$TARGET"
echo "==> packaging $pkg"

# --- build -------------------------------------------------------------------

if [ "$SKIP_BUILD" = "1" ]; then
  echo "==> SKIP_BUILD=1, reusing target/$TARGET/release"
else
  # --locked: the archive is built from the committed Cargo.lock, exactly like
  # the crates published from the same tag.
  # strip=symbols via --config rather than the manifest, so the release profile
  # is unchanged for every other consumer of this workspace.
  cargo build --release --locked --target "$TARGET" \
    --config 'profile.release.strip="symbols"' \
    -p gonzalo-cli -p gonzalo-mcp -p gonzalo-parse
fi

# --- stage and sign ----------------------------------------------------------

rm -rf "dist/$pkg"
mkdir -p "dist/$pkg"
for bin in "${BINS[@]}"; do
  cp "target/$TARGET/release/$bin" "dist/$pkg/"
done
cp LICENSE "dist/$pkg/"

# Apple Silicon refuses to exec an unsigned Mach-O. The linker adds an ad-hoc
# signature, but stripping symbols invalidates it and the binary then dies with
# `Killed: 9` and no legible error. Re-sign ad-hoc, then prove it took.
for bin in "${BINS[@]}"; do
  codesign --force --sign - "dist/$pkg/$bin"
  codesign --verify "dist/$pkg/$bin"
done

# --- smoke-test what is actually about to ship -------------------------------

# Each binary gets the cheapest check that proves it executes. They differ
# because only the CLI takes arguments: `gonzalo-mcp` and `gonzalo-parse-worker`
# are stdin-driven servers, which would read a `--version` flag as a request.
#
# Both stdin-driven checks run under a watchdog. macOS ships no coreutils
# `timeout`, and a server that failed to notice EOF would otherwise hang here —
# on a runner, for the job's full 60-minute budget.
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

run_briefly() { # <seconds> <label> <command…> — kill and fail on overrun
  local secs="$1" label="$2"
  shift 2
  "$@" &
  local pid=$! waited=0
  while kill -0 "$pid" 2>/dev/null; do
    if [ "$waited" -ge "$secs" ]; then
      kill -9 "$pid" 2>/dev/null || true
      echo "error: $label did not exit within ${secs}s" >&2
      return 1
    fi
    sleep 1
    waited=$((waited + 1))
  done
  wait "$pid"
}

echo "==> smoke test: gonzalo --version"
out="$("dist/$pkg/gonzalo" --version)"
echo "    $out"
case "$out" in
  *"$version"*) ;;
  *) echo "error: gonzalo reports '$out', expected $version" >&2; exit 1 ;;
esac

# One real parse through the worker: this is the only check that proves the
# tree-sitter C grammars linked and load, which is the whole reason the worker
# exists. `b()` must come back as a reference.
echo "==> smoke test: gonzalo-parse-worker round-trip"
printf '%s\n' '{"language":"Rust","source":"fn a() { b(); }"}' > "$scratch/probe.json"
worker_probe() {
  "dist/$pkg/gonzalo-parse-worker" < "$scratch/probe.json" > "$scratch/graph.json"
}
run_briefly 30 gonzalo-parse-worker worker_probe
graph="$(cat "$scratch/graph.json")"
case "$graph" in
  *'"b"'*) echo "    parsed: $graph" ;;
  *) echo "error: worker returned no reference to b(): ${graph:-<no output>}" >&2; exit 1 ;;
esac

# The MCP server speaks JSON-RPC over stdio. An `initialize` handshake proves the
# binary runs and answers; the already-closed stdin then ends it. GONZALO_ROOT
# points into the scratch directory so this cannot touch a real store.
echo "==> smoke test: gonzalo-mcp initialize"
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"package-macos.sh","version":"0"}}}' > "$scratch/init.json"
mcp_probe() {
  GONZALO_ROOT="$scratch/store" "dist/$pkg/gonzalo-mcp" \
    < "$scratch/init.json" > "$scratch/reply.json" 2>/dev/null
}
run_briefly 30 gonzalo-mcp mcp_probe
case "$(head -1 "$scratch/reply.json")" in
  *'"result"'*) echo "    handshake ok" ;;
  *) echo "error: gonzalo-mcp did not answer initialize" >&2
     cat "$scratch/reply.json" >&2
     exit 1 ;;
esac

# --- archive -----------------------------------------------------------------

tar -C dist -czf "dist/$pkg.tar.gz" "$pkg"
( cd dist && shasum -a 256 "$pkg.tar.gz" > "$pkg.tar.gz.sha256" )

echo "==> done"
ls -lh "dist/$pkg.tar.gz" "dist/$pkg.tar.gz.sha256"
cat "dist/$pkg.tar.gz.sha256"
