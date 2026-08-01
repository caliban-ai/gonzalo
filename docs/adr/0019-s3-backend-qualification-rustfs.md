# ADR 0019 · Qualified S3 backend for HA: RustFS

- **Status:** accepted
- **Date:** 2026-08-01
- **Source:** [`docs/superpowers/specs/2026-07-11-gonzalo-ha-soak-design.md`](../superpowers/specs/2026-07-11-gonzalo-ha-soak-design.md) §"Implementation finding"

## Context

ADR 0004 made the backend a configuration choice behind one `Store` trait, and
its "revisit if" named the failure mode precisely: *a needed backend cannot
satisfy the `Store` contract*. Multi-replica `gonzalod` over an S3-compatible
store is the first case that hit it.

The contract at stake is optimistic concurrency (ADR 0005): concurrent edits are
never silently lost, and a loser is told (`PutResult::Conflict`). Over S3 that
reduces to one requirement — **atomic `If-Match` conditional writes**. Without
them the store degrades to check-then-set, and lost updates are silent, which is
the one outcome the core exists to prevent.

"S3-compatible" turned out not to imply this. `gonzalo-store-s3`'s own
conformance case (`concurrent_updates_with_same_expected_let_exactly_one_win`,
ADR 0006) expects exactly 1 of 8 racers to commit, and became a backend
qualifier. Running it against candidates:

| Backend | Atomic `If-Match` | License | Outcome |
|---|---|---|---|
| **RustFS** `1.0.0-beta.8` | ✅ deterministic (3/3 + full soak) | Apache-2.0 | **chosen** — Rust, MinIO-compatible, drop-in |
| MinIO | ✅ | AGPL | rejected — project sustainability |
| Garage | ❌ non-atomic | AGPL | disqualified — see below |
| SeaweedFS | ⚠️ setup-blocked, upstream CAS bugs | Apache-2.0 | not pursued |
| Ceph RGW | not tested | LGPL | heavyweight, against the modest-hardware goal |

**Garage** fails the invariant outright. v1.0.1 let **8/8** racers commit; v2.1.0
let **3–8** commit non-deterministically — the signature of check-then-set rather
than atomic CAS. Its own S3-compatibility reference documents no
`If-Match`/`If-None-Match` for `PutObject`, and v2.1.0 exposes no consistency or
quorum setting that changes this. It is a design limitation, not a tunable, so
there is no configuration under which Garage is safe for gonzalo.

**MinIO** passes the qualifier and is technically sound — it served as the
control that proved the harness itself was correct. It is rejected on
sustainability rather than correctness: through 2025 the project moved
functionality out of the community edition toward its commercial offering and
curtailed community development. Betting the persistence tier of an AGPL-3.0
project on a vendor actively narrowing what its open edition does is a risk we
decline to take, independent of today's licence text.

## Decision

We will treat **atomic `If-Match` as a hard qualification gate** for any
S3-compatible backend, and qualify backends by running the existing
conditional-write conformance case against them — not by reading compatibility
matrices.

**RustFS is the qualified S3 backend** for multi-replica HA: the only FOSS S3
store that passes, Rust, Apache-2.0, and drop-in MinIO-compatible. The HA soak
provisions it (`docker-compose.rustfs.yml`, `scripts/rustfs-up.sh`).

RustFS is pre-1.0 (beta), so this is a near-term answer, not a permanent one. The
mature foundation is a **Postgres substrate** (`gonzalo-store-postgres`, native
atomic CAS, aligned with prospero's clustered tier), tracked separately.

Garage's compose setup is retained **solely as a reproducer of the finding**, not
as a supported backend. Nothing in the soak harness is backend-specific — it
reads an S3 endpoint — so qualifying a new candidate is a matter of pointing it
at one.

This does not narrow ADR 0004: the substrate remains configuration, and `fs`
remains the zero-dependency default. It records which concrete S3 *servers* clear
the contract that ADR 0004 requires every substrate to meet.

## Consequences

- **Positive:** HA rests on a backend proven against gonzalo's own concurrency
  invariant rather than a vendor's compatibility claim. The qualifier is
  reusable, so future candidates are a test run, not an investigation. Both the
  chosen backend and the toolchain stay Rust and permissively licensed.
- **Negative:** we depend on a pre-1.0 beta for the HA path, and inherit its
  stability risk until the Postgres substrate lands. The qualification gate rules
  out most of the S3-compatible ecosystem, so "any S3 store" is a claim we can no
  longer make. Documents that named Garage as the target — the k8s system design
  and the gonzalo chart — are now wrong and need correcting.
- **Revisit if:** RustFS reaches 1.0 (upgrade from tolerated to preferred) or
  stalls; the Postgres substrate lands and supersedes it for HA; Garage or
  SeaweedFS ships atomic conditional writes and passes the qualifier; or a
  backend passes the qualifier but fails the invariant in the full soak, which
  would mean the qualifier is too weak.
