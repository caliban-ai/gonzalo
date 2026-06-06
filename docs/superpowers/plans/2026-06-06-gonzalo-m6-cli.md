# Gonzalo M6 — CLI + caliban integration (Design & Build Notes)

- **Date:** 2026-06-06
- **Status:** `gonzalo-cli` implemented on `main`; caliban-side integration scoped (see below).
- Implements design spec §3 (`gonzalo-cli`) and §11 (migration enabler).

## `gonzalo-cli`

An admin/ops binary (`gonzalo`) over a filesystem store, with command logic in
the crate library (unit-tested directly) and a thin `clap` front-end:

- **`list --root <dir> [--namespace N] [--collection C]`** — print record keys.
- **`get --root <dir> <ns> <col> <id>`** — print a record as pretty JSON.
- **`status --root <dir>`** — record counts grouped by `namespace/collection`.
- **`migrate --root <dir> <src> --namespace N --collection C [--kind K]`** —
  recursively import every file under `src` as a record. The id is
  `segment(<relative path>)` so it is stable and round-trips through the fs
  layout; body = file bytes. **Idempotent**: existing keys are skipped.
- **`sync <a> <b>`** — reconcile two filesystem stores via `gonzalo_core::sync`,
  printing the copied/merged/conflict counts.

Tested: migrate (incl. idempotency), list, get, status grouping, and sync
copy — 6 tests; clippy/fmt clean.

`migrate` is the **gonzalo-side enabler** for adopting gonzalo in caliban: an
operator can import caliban's existing on-disk memory/sessions/checkpoints into
gonzalo records today.

## caliban integration — scope

Design spec §11 calls for caliban to swap its direct file I/O in
`caliban-memory` / `caliban-sessions` / `caliban-checkpoint` for the `gonzalo`
facade. That change lives in the **separate `caliban-ai/caliban` repository** —
a daily-usable project with its own architecture and release posture — so it is
a distinct, cross-repo effort rather than part of the gonzalo workspace. It is
intentionally **not** performed here. Gonzalo now provides everything that swap
needs:

- the `gonzalo` facade with an `fs` substrate that mirrors caliban's on-disk
  layout (default behavior unchanged), plus `git`/`s3`/`remote` substrates and
  `vector`/`graph` layers behind features;
- typed domain views (`MemoryTier`, `Topic`, `Session`, `Checkpoint`);
- `gonzalo-cli migrate` to import existing data.

The caliban-side work (replacing its I/O calls, choosing a substrate via
caliban settings, and running `migrate`) should be planned and executed in the
caliban repo as its own spec → plan → implementation cycle.

## Milestone status

With M6, the gonzalo workspace implements the full design spec across all
in-repo milestones (M1 foundation, M2 git/s3 + sync, M3 daemon, M4 vector,
M5 code graph, M6 CLI). Remaining work is the cross-repo caliban adoption and
the explicitly-deferred refinements noted in each milestone's design notes
(3-way merge with stored ancestry, native S3 conditional writes, approximate
vector indexes, resolved name graph, namespace-scoped daemon auth).
