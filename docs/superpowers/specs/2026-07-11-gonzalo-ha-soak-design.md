# Design: gonzalo HA soak — stateless `gonzalod` replicas over Garage

**Status:** approved (brainstorming), pending implementation plan
**Date:** 2026-07-11
**Ticket:** gonzalo #52 (part of the k8s epic #274, phase P3)
**Depends on:** gonzalo #5 — native S3 conditional writes (`If-Match`/`If-None-Match`), **shipped in v0.2.0** (the correctness gate this test exercises)
**Upstream spec:** `caliban-ai/docs/superpowers/specs/2026-07-03-caliban-k8s-system-design.md` §5, "High availability summary", "Dev/test"

## Implementation finding (2026-07-12): Garage is unsuitable; backend is RustFS

Building the soak surfaced a **critical finding**: **Garage does not provide atomic
`If-Match` conditional writes**, so gonzalo's core invariant (concurrent edits are
never silently lost) **breaks on Garage**. Evidence:

- gonzalo's own S3 conditional-write conformance
  (`gonzalo-store-s3` `concurrent_updates_with_same_expected_let_exactly_one_win`)
  **fails against Garage**: v1.0.1 lets **8/8** concurrent racers commit; v2.1.0
  lets **3–8** commit (non-deterministic) — the signature of a TOCTOU check-then-set,
  not an atomic CAS. Expected: exactly **1**.
- Garage's own [S3-compatibility reference](https://garagehq.deuxfleurs.fr/documentation/reference-manual/s3-compatibility/)
  documents no `If-Match`/`If-None-Match` support for `PutObject`; Garage v2.1.0
  exposes no consistency/quorum config to change this. It is a fundamental
  limitation, not a tunable.

**Backend qualification.** The soak is a backend-agnostic conditional-write
qualifier; running it against candidates gave:

| Backend | Atomic `If-Match` | License | Notes |
|---|---|---|---|
| **RustFS** `1.0.0-beta.8` | ✅ deterministic (qualifier 3/3 + full soak) | Apache-2.0 | Rust, MinIO-compatible; **chosen backend** (pre-1.0 beta) |
| MinIO | ✅ | AGPL | control that proved the harness; **rejected — sustainability** (2025 community gutting) |
| SeaweedFS | ⚠️ setup-blocked + upstream CAS bugs | Apache-2.0 | S3 auth needs identity config; not pursued |
| Garage | ❌ non-atomic | AGPL | the finding above |
| Ceph RGW | not tested | LGPL | heavyweight — against the "modest hardware" goal |

**Decision:** the soak's backend is **RustFS** (`docker-compose.rustfs.yml` +
`scripts/rustfs-up.sh`) — the only FOSS S3 store that passes, Rust + Apache-2.0,
drop-in. It is **pre-1.0 (beta)**, so a **Postgres substrate**
(`gonzalo-store-postgres`, native atomic CAS, aligns with prospero's clustered
tier) is tracked as the mature long-term HA backend. The Garage setup
(`docker-compose.garage.yml` + `scripts/garage-up.sh`) is retained only as a
**reproducer of the finding**.

**Consequence for the k8s epic (#274):** design §5 chose **Garage** as the gonzalo
HA backend — that choice is **unsound**. Reported back with the recommendation:
RustFS near-term, a Postgres substrate as the robust foundation.

The Rust harness is backend-agnostic (it reads an S3 endpoint), so nothing in it
changed across backends. Everywhere below that says "Garage", read "the S3
backend (RustFS)".

## Problem

The k8s system design makes an HA claim for the persistence tier:

```
        ┌─ gonzalod replica 1 ─┐
k8s Svc ┼─ gonzalod replica 2 ─┼──▶  Garage (S3-compatible), objects replicated across nodes
        └─ gonzalod replica 3 ─┘
```

**Compute HA** = N *stateless* `gonzalod` replicas behind a Service. **Data HA** =
the object store's replication. The **correctness lynchpin** (the spec's word):
multiple replicas racing on one record must get conditional writes right, or
gonzalo's core invariant — *concurrent edits are never silently lost*
(`PutResult::Conflict`) — breaks. #5 shipped the conditional writes; **#52 is the
test that proves the invariant holds under real concurrency and replica failure.**

Today nothing exercises multiple `gonzalod` replicas against a shared S3 backend
under load + chaos. This ticket builds that soak.

## Goals

1. Prove **safety** under concurrent multi-replica load: no lost updates; racing
   writers observe `Conflict` (never a silent overwrite).
2. Prove **durability under churn**: no acked write is lost when replicas die.
3. Prove **liveness**: the workload keeps making progress across replica kills —
   the remote-substrate client fails over to a surviving replica.
4. Be a **per-PR regression gate** (bounded, ~60–90s) for the conditional-write
   invariant, plus a **deep soak** for manual/nightly runs.

## Non-goals

- **Testing Garage itself.** Object-store durability under Garage *node* loss is
  Garage's concern; we trust S3 durability. Chaos targets `gonzalod` replicas.
  A multi-node-Garage / Garage-node-kill variant is a later add-on for the deep
  soak, not part of the core invariant. Single-node Garage suffices here.
- **A full linearizability checker** (Jepsen/elle-style history checking). A
  conditional-write CAS register is provable with a targeted invariant oracle
  (below); full history-checking is out of scope as overkill.
- **k8s / pod / node chaos.** This soak runs against real `gonzalod` subprocesses
  driven through the same remote substrate k8s uses, but does not stand up a
  cluster. (A k8s-level soak is a possible future sibling in `helm-charts`.)

## Decisions (locked during brainstorming)

1. **Harness = Rust test + local Garage**, in the gonzalo workspace (closest to
   the existing store-conformance style; CI-runnable).
2. **Two-tier**: a bounded `#[test]` per-PR gate + a parameterized deep-soak `bin`.
3. **Replica model = real `gonzalod` subprocesses** driven via the
   `gonzalo-store-server` remote client — the exact path k8s agent pods use.
   ("Kill" = `SIGKILL` a subprocess; "recover" = respawn.)
4. **Provisioning = external, env-driven, skip-if-unset** — mirrors the existing
   `gonzalo-store-s3/tests/integration.rs` convention; the Rust code never
   orchestrates containers.
5. **Chaos scope = gonzalod replica kills** (see Non-goals re: Garage nodes).
6. **Safety oracle = invariant checker**, not a linearizability checker.

## Architecture

New workspace crate **`gonzalo-soak`**:

```
crates/gonzalo-soak/
  Cargo.toml
  src/
    lib.rs        # the shared harness (below)
    main.rs       # bin `gonzalo-soak` — the deep/parameterized soak
  tests/
    ha_soak.rs    # the bounded per-PR gate #[test] (skips if no Garage)
docker-compose.garage.yml   # single-node Garage
scripts/garage-up.sh        # compose up + layout-assign + bucket/key bootstrap → exports env
.github/workflows/ha-soak.yml  # dedicated CI job (provision Garage → run the gate)
```

### Harness components (`lib.rs`)

- **`GarageTarget`** — reads `GONZALO_S3_TEST_ENDPOINT`, `GONZALO_S3_TEST_BUCKET`,
  and `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` from the env; returns `None`
  (→ graceful skip) when unset, exactly like the existing S3 integration test.
- **`ReplicaSet`** — spawns N real `gonzalod` subprocesses via `std::process::Command`,
  each with `GONZALO_STORE=s3`, the shared `GONZALO_S3_BUCKET`/`GONZALO_S3_ENDPOINT`,
  AWS creds, and distinct `GONZALO_HTTP_ADDR`/`GONZALO_GRPC_ADDR` (localhost, unique
  ports). Waits for each replica's `/readyz` (shipped #63) before load. Exposes
  `kill(i)` (`SIGKILL`) and `spawn(i)` (respawn). Owns process cleanup on drop.
  The `gonzalod` binary is located via `CARGO_BIN_EXE_gonzalod` when run as a test
  of the server crate, or built/looked-up on `PATH`/target dir for the bin.
- **`Dispatcher`** — the stand-in for the k8s Service: holds one
  `ServerStore::http("http://127.0.0.1:<port>")` per replica, round-robins each
  op across *live* replicas, and on a connection error retries against another
  replica (bounded). This is what exercises remote-substrate failover.
- **`Workload`** — spawns W concurrent writer tasks (tokio) driving ops through the
  `Dispatcher`. Two op streams (see oracle): contended RMW on shared keys, and
  unique-key writes. Records every op's outcome (Committed rev / Conflict / error).
- **`SafetyOracle`** — post-run assertions over the recorded outcomes + a final
  read of every key.

### The safety oracle (checkable invariants)

**(a) Arbitration / no-lost-update — contended keys.** W writers loop RMW on a
small set of *shared* keys: read current record (revision `R`) → append their
unique `op-id` to the record's set-valued body → `put(expected_parent = R)`. On
`PutResult::Conflict`, re-read and retry (bounded attempts).

- **Assert:** for each shared key, the final record's set contains *every*
  committed `op-id` for that key **exactly once**, and the revision-chain length
  equals the number of committed puts. ⇒ no committed update was silently lost.
- **Assert:** the run observed a non-zero number of `Conflict` outcomes on the
  contended keys (proves racers genuinely serialize through conditional writes,
  not silent last-writer-wins). Under real contention across replicas this is
  expected; if zero, the test fails loudly (the invariant wasn't actually
  exercised).

**(b) Durability under churn — unique keys.** Writers also write disjoint,
unique keys (no contention). **Assert:** after all chaos, every *acked* unique
write is readable with its exact value. ⇒ no acked write lost across replica kills.

**(c) Liveness.** Throughput stays > 0 across kill cycles and all writer tasks
complete within the deadline. ⇒ a single replica death does not fail an op (the
Dispatcher fails over). A stalled/timed-out workload fails the test.

### Chaos schedule

- **Bounded gate:** start load → after warm-up, `kill(1)` → continue load →
  `spawn(1)` + wait `/readyz` → drain → assert. One kill+recover cycle.
- **Deep soak:** a chaos loop on `--chaos-interval` that kills a random live
  replica (occasionally two at once, keeping ≥1 alive), recovers it after a
  hold, for `--duration`.

## Two tiers

| | PR gate (`tests/ha_soak.rs`) | Deep soak (`bin gonzalo-soak`) |
|---|---|---|
| replicas | 3 | `--replicas` (default 3) |
| writers | ~8 | `--writers` (default 16) |
| shared keys | few (e.g. 4) | `--shared-keys` |
| chaos | 1 kill+recover | loop on `--chaos-interval`, multi-kill |
| duration | ~60–90s | `--duration` (e.g. 30m) |
| when | dedicated CI job on PRs + local | manual / nightly `schedule` |
| skip | yes, if `GONZALO_S3_TEST_ENDPOINT` unset | errors if unset (explicit run) |

Both share the exact `lib.rs` harness + oracle — one code path, two drivers.

## CI wiring

A dedicated **`.github/workflows/ha-soak.yml`** job (on `pull_request` + optional
nightly `schedule`):

1. `bash scripts/garage-up.sh` — compose up single-node Garage, `garage layout
   assign` a node, create the test bucket + an access key, emit the env exports.
2. Export `GONZALO_S3_TEST_ENDPOINT` / `GONZALO_S3_TEST_BUCKET` / `AWS_*`.
3. `cargo test -p gonzalo-soak --test ha_soak`.

Kept **out** of the fast `fmt·clippy·build·test` job: a Garage/chaos hiccup must
not block unrelated PRs, and because the gate test skips-when-unprovisioned,
`cargo test --workspace` (no docker) stays green. GitHub-hosted ubuntu runners
have docker preinstalled, so the dedicated job can compose-up Garage.

## Error handling & flake control

- **Graceful skip** when Garage env is unset (both the `#[test]` and a `--require`
  guard on the bin) — no false failures on machines without docker.
- **Bounded retries** on `Conflict` (RMW re-read loop) and on transient
  connection errors during a replica kill — with a hard cap so a genuine hang
  still fails.
- **Generous, explicit timeouts** on `/readyz` waits and overall workload
  completion; the gate's budget is bounded (~90s) so a stuck run fails fast.
- **Deterministic-enough:** the oracle asserts on invariants (set membership,
  chain length, liveness), never on exact interleavings, so normal scheduling
  jitter never flakes it.
- **Full process cleanup:** `ReplicaSet` kills all children on drop; the CI job
  tears down the compose stack in an `always()` step.

## Open questions (resolve at plan/implementation time, not blocking)

1. **Binary discovery** for spawned `gonzalod` — `CARGO_BIN_EXE_gonzalod` is only
   injected for tests *in the server crate*. Options for `gonzalo-soak`: a
   `build`-dependency/`env!` shim, look up `target/<profile>/gonzalod`, or a
   thin `escargot`/`cargo build` step at harness init. Pick the least-magic one.
2. **Auth on/off** for the replicas — run open (simplest) or with a shared
   `GONZALO_TOKEN` to also exercise the authed path. Default: open for the gate,
   `--token` optional in the bin.
3. **HTTP vs gRPC** in the `Dispatcher` — start with HTTP (`ServerStore::http`,
   matches the `/readyz` path); gRPC is a later parameterization if wanted.
4. **Workspace membership vs exclude** — member (so the gate runs in the CI job's
   `cargo test -p`), but confirm its dev-deps don't bloat the normal build; if
   they do, consider `exclude` + explicit `-p` like `gonzalo-vector-bench`.

## Affected files / new artifacts

- **new** `crates/gonzalo-soak/` (lib + bin + `tests/ha_soak.rs`)
- **new** `docker-compose.garage.yml`, `scripts/garage-up.sh`
- **new** `.github/workflows/ha-soak.yml`
- **edit** `Cargo.toml` workspace members
- **edit** `charts/gonzalo/README.md` (helm-charts) — the HA note that cites #5 as
  "not shipped" is now stale; once this soak green-lights multi-replica HA, update
  that rationale. (Tracked separately; flagged during the v0.2.0 tag bump.)

## References

- k8s system design §5, HA summary, Dev/test, P3.
- gonzalo #5 (conditional writes), #62 (env substrate selection), #63 (`/readyz`).
- Existing pattern: `crates/gonzalo-store-s3/tests/integration.rs` (env-driven,
  skip-if-unset), `crates/gonzalo-core/src/conformance.rs` (invariant-oracle style).
