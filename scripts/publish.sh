#!/usr/bin/env bash
#
# Resumable, rate-limit-aware crates.io publisher for the gonzalo workspace.
#
# Why this exists: crates.io limits *new crate* creation to a burst of 5, then
# ~1 per 10 minutes. Publishing all of gonzalo's crates for the first time
# exceeds that, so a single `cargo publish --workspace` fails partway with HTTP
# 429 (and is not resumable — it errors on the crates already uploaded).
#
# This script:
#   - publishes crates in dependency order, one at a time, with --no-verify
#     (CI's package-check already verified packaging on the release commit, so
#     re-compiling each crate at publish time is pure waste);
#   - skips crates already live at the target version, so it is idempotent and
#     resumable — safe to Ctrl-C and re-run, or to run after a partial publish;
#   - on a 429, parses crates.io's "try again after" time and waits until then.
#
# Run it LOCALLY for the first publish so the multi-hour pacing costs no CI
# minutes (sleeps on your machine are free; sleeps on a GitHub runner are billed).
# In CI, set MAX_SLEEP_SECS=0 so the runner never idles on a 429 — it publishes
# what the burst allows and exits, and you resume locally.
#
# Auth: run `cargo login` first, or export CARGO_REGISTRY_TOKEN.
#
set -uo pipefail

UA="gonzalo-publish (john.ford2002@gmail.com)"
MAX_SLEEP_SECS="${MAX_SLEEP_SECS:-86400}"   # cap on a single 429 wait; CI sets 0
FALLBACK_SLEEP="${FALLBACK_SLEEP:-615}"      # 10 min + buffer if no time is parsed

# jq locally may be jaq (a jq clone); GitHub runners ship jq. Use whichever
# exists — the filters below are common to both.
JQ="$(command -v jq || command -v jaq)"
[ -n "$JQ" ] || { echo "need jq or jaq on PATH" >&2; exit 1; }

meta() { cargo metadata --no-deps --format-version 1; }
# The workspace version is shared by every member; read it off gonzalo-core (a
# leaf lib present in every publishable configuration), matching publish.yml's
# tag↔version guard.
VERSION="$(meta | "$JQ" -r '.packages[] | select(.name=="gonzalo-core") | .version')"
[ -n "$VERSION" ] || { echo "could not read workspace version" >&2; exit 1; }
echo "==> publishing gonzalo workspace @ ${VERSION}  (resumable, --no-verify)"

# Dependency-ordered crate list (deps first) via tsort over intra-workspace
# (path) dependencies. Crates marked `publish = false` are excluded up front so
# they are never uploaded. Isolated nodes are appended so nothing is dropped.
PUBLISHABLE="$(meta | "$JQ" -r '.packages[] | select(.publish != []) | .name')"
is_publishable() { printf '%s\n' "$PUBLISHABLE" | grep -qx "$1"; }

edges() { meta | "$JQ" -r '.packages[] as $p
  | ($p.dependencies[]? | select(.path != null) | "\(.name) \($p.name)")'; }
ORDER="$(edges | tsort)"
for m in $PUBLISHABLE; do
  printf '%s\n' "$ORDER" | grep -qx "$m" || ORDER="${ORDER}
${m}"
done

is_published() { # 0 if $1@$VERSION is already on crates.io
  curl -sS --max-time 25 -H "User-Agent: ${UA}" \
    "https://crates.io/api/v1/crates/$1/${VERSION}" \
    | "$JQ" -e '.version.num? // empty' >/dev/null 2>&1
}

wait_secs() { # echo seconds to wait until RFC2822 time in $1 (GNU or BSD date)
  local when="$1" tgt now
  tgt="$(date -u -d "$when" +%s 2>/dev/null)" \
    || tgt="$(date -j -f '%a, %d %b %Y %H:%M:%S %Z' "$when" +%s 2>/dev/null)" \
    || tgt=""
  now="$(date +%s)"
  if [ -n "$tgt" ]; then echo $(( tgt - now + 15 )); else echo "$FALLBACK_SLEEP"; fi
}

done_count=0; skip_count=0
for crate in $ORDER; do
  # tsort may surface a path-dep that is itself publish=false (e.g. a dev-only
  # harness other crates dev-depend on) — never upload those.
  is_publishable "$crate" || { echo "  · ${crate} publish=false — skip"; continue; }
  if is_published "$crate"; then
    echo "  ✓ ${crate} already on crates.io — skip"; skip_count=$((skip_count+1)); continue
  fi
  while true; do
    echo "  → [$(date +%H:%M:%S)] publishing ${crate} ..."
    out="$(cargo publish -p "$crate" --no-verify 2>&1)"; rc=$?
    if [ $rc -eq 0 ]; then echo "    ✓ ${crate} published"; done_count=$((done_count+1)); break; fi
    printf '%s\n' "$out" | sed 's/^/      | /'
    if printf '%s' "$out" | grep -qiE 'already (been )?uploaded|already exists|is already'; then
      echo "    ✓ ${crate} already present — skip"; skip_count=$((skip_count+1)); break
    fi
    if printf '%s' "$out" | grep -qiE 'status 429|too many'; then
      when="$(printf '%s' "$out" | sed -n 's/.*try again after \(.*GMT\).*/\1/p' | head -1)"
      secs="$(wait_secs "$when")"; [ "$secs" -lt 30 ] && secs="$FALLBACK_SLEEP"
      if [ "$secs" -gt "$MAX_SLEEP_SECS" ]; then
        echo "    ⏸ 429: need to wait ${secs}s (> MAX_SLEEP_SECS=${MAX_SLEEP_SECS}). Stopping; re-run to resume."
        echo "==> partial: ${done_count} published, ${skip_count} already present this run."
        exit 75   # EX_TEMPFAIL: more to do, came back later
      fi
      echo "    ⏳ 429 rate limit — sleeping ${secs}s, then retrying ${crate}"
      sleep "$secs"; continue
    fi
    echo "    ✗ ${crate}: non-rate-limit error (see above). Aborting." >&2; exit 1
  done
done
echo "==> done: ${done_count} published, ${skip_count} already present. All crates @ ${VERSION}."
