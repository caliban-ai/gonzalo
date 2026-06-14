#!/usr/bin/env bash
# Measure workspace test coverage and enforce a minimum line-coverage
# threshold. This is the single entrypoint used by both humans and CI
# (.github/workflows/ci.yml), so the local and CI code paths are identical.
#
# Why this exists: gonzalo had no coverage visibility and no guard against
# regressions. This script runs cargo-llvm-cov over the whole workspace
# (matching `cargo test --workspace --all-features`) and fails when line
# coverage drops below COVERAGE_MIN — a ratchet that stops new work from
# silently eroding test coverage.
#
# Tooling: cargo-llvm-cov (LLVM source-based coverage). Install with
#   cargo install cargo-llvm-cov --locked
# and provide the LLVM coverage tools. On a rustup toolchain (CI):
#   rustup component add llvm-tools-preview
# On a Homebrew Rust toolchain (no rustup), point cargo-llvm-cov at Homebrew's
# LLVM instead — this script auto-detects it (see below), or set manually:
#   export LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov
#   export LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata
#
# Usage:
#   scripts/coverage.sh              # summary + lcov artifact, enforce threshold
#   scripts/coverage.sh --html       # also write an HTML report, enforce threshold
#   scripts/coverage.sh --open       # --html, then open the report in a browser
#   scripts/coverage.sh --no-fail    # report only; never fail on low coverage
#   scripts/coverage.sh -h | --help
#
# Environment:
#   COVERAGE_MIN   minimum line-coverage percent (default below). Override to
#                  ratchet the floor up over time, e.g. COVERAGE_MIN=82 scripts/coverage.sh
#
# Outputs (under target/llvm-cov/):
#   target/llvm-cov/lcov.info      LCOV report (consumed by CI artifact)
#   target/llvm-cov/coverage.json  JSON export (rendered by scripts/coverage-report.py)
#   target/llvm-cov/html/          HTML report (only with --html / --open)
#
# Exit code is non-zero when coverage is under COVERAGE_MIN (unless --no-fail).

set -euo pipefail

cd "$(dirname "$0")/.."

# Baseline floor — the single source of truth for the coverage gate. CI
# (.github/workflows/ci.yml) calls this script without overriding COVERAGE_MIN,
# so this default governs both local and CI runs. Start at/just below the
# current measured coverage and ratchet upward over time as tests are added.
#
# Ratchet history:
#   80  initial rollout — just below the 80.90% baseline (2026-06-13), with the
#       daemon `gonzalod` entrypoint excluded from the denominator. Lowest crates
#       at rollout: store-s3 (31.9%), store-git (50.5%), server/grpc.rs (41.4%).
COVERAGE_MIN="${COVERAGE_MIN:-80}"

# Local convenience: on a Homebrew Rust toolchain there is no rustup
# llvm-tools-preview; point cargo-llvm-cov at Homebrew's LLVM if it's present and
# the caller hasn't already set the tools. CI installs rustup's
# llvm-tools-preview, so this block is a no-op there.
if [[ -z "${LLVM_COV:-}" && -x /opt/homebrew/opt/llvm/bin/llvm-cov ]]; then
    export LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov
    export LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata
fi

DO_HTML=0
DO_OPEN=0
DO_FAIL=1

for arg in "$@"; do
    case "$arg" in
        --html)    DO_HTML=1 ;;
        --open)    DO_HTML=1; DO_OPEN=1 ;;
        --no-fail) DO_FAIL=0 ;;
        -h|--help)
            sed -n '2,38p' "$0"
            exit 0
            ;;
        *)
            echo "unknown flag: $arg" >&2
            exit 2
            ;;
    esac
done

if ! cargo llvm-cov --version >/dev/null 2>&1; then
    cat >&2 <<'MSG'
error: cargo-llvm-cov is not installed.

  cargo install cargo-llvm-cov --locked
  rustup component add llvm-tools-preview   # or set LLVM_COV / LLVM_PROFDATA

See https://github.com/taiki-e/cargo-llvm-cov for details.
MSG
    exit 127
fi

run() {
    echo "==> $*"
    "$@"
}

# The daemon's `gonzalod` entrypoint (signal handling + blocking serve loop) is
# excluded from the coverage denominator: `cargo test` never executes a bin
# `main`, so unit/integration coverage there is low-value and would only depress
# the ratio. The CLI `main` is intentionally NOT excluded — its subcommand
# dispatch is testable.
IGNORE_REGEX='crates/gonzalo-server/src/bin/gonzalod\.rs'

# Whole-workspace coverage with every substrate/capability feature enabled, so
# the facade's feature-gated re-exports are instrumented too.
FEATURE_ARGS=(--all-features)

OUT_DIR="target/llvm-cov"
LCOV_PATH="$OUT_DIR/lcov.info"
JSON_PATH="$OUT_DIR/coverage.json"

# cargo-llvm-cov does not create the parent dir for a custom --output-path.
mkdir -p "$OUT_DIR"

echo "coverage floor: ${COVERAGE_MIN}% line coverage (COVERAGE_MIN)"

# Gather coverage once (runs the workspace test suite) and write the LCOV
# artifact. The threshold is enforced as a separate final step below, so the
# reports always exist even when the gate fails — that's exactly when the PR
# comment / gap report is most useful.
run cargo llvm-cov --workspace "${FEATURE_ARGS[@]}" \
    --ignore-filename-regex "$IGNORE_REGEX" --lcov --output-path "$LCOV_PATH"

# JSON export that scripts/coverage-report.py renders into the Markdown PR
# comment / job summary. Reuses the profile data above (no re-test).
run cargo llvm-cov report --ignore-filename-regex "$IGNORE_REGEX" \
    --json --output-path "$JSON_PATH"

if [[ $DO_HTML -eq 1 ]]; then
    run cargo llvm-cov report --ignore-filename-regex "$IGNORE_REGEX" \
        --html --output-dir "$OUT_DIR"
    echo "HTML report: $OUT_DIR/html/index.html"
    if [[ $DO_OPEN -eq 1 ]]; then
        open "$OUT_DIR/html/index.html" 2>/dev/null \
            || xdg-open "$OUT_DIR/html/index.html" 2>/dev/null \
            || echo "open the report manually: $OUT_DIR/html/index.html"
    fi
fi

echo
echo "coverage reports written to $LCOV_PATH and $JSON_PATH"

# Gate last: reuses the gathered data to print the summary table and fail when
# line coverage is under the floor. Reports above are already on disk.
if [[ $DO_FAIL -eq 1 ]]; then
    run cargo llvm-cov report --summary-only \
        --ignore-filename-regex "$IGNORE_REGEX" --fail-under-lines "$COVERAGE_MIN"
fi
