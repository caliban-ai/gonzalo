# Tombstones Slice 7: Soak and Docs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove replicated deletion under multi-replica chaos in the HA soak, and document it: ADR 0021, the ADR 0018 back-reference, a user guide page, CHANGELOG, and follow-up tickets.

**Architecture:** The soak's writers gain a third op stream. It runs edit, delete and recreate ops against a small set of `life-*` keys, and deletes some of each writer's unique keys. Once the writers stop and every replica is live again, the harness reads each lifecycle key from **every** replica with `get_raw` and `get`. The pure oracle then checks three things: every key is live everywhere or a tombstone everywhere, all replicas report the same revision, and each replica's consumer read matches its raw read. The docs are plain Markdown in the existing ADR log, the mdBook guide and `CHANGELOG.md`.

**Tech Stack:** Rust 2024 (tokio, async-trait), the `gonzalo-soak` crate, RustFS for the S3-backed gate, mdBook 0.4.52, `gh` CLI.

**Spec:** `docs/superpowers/specs/2026-09-13-tombstone-replication-design.md` (§4.1, §4.3, §5, §6.7, §7 item 8, §8). The shared contract is in `docs/superpowers/plans/2026-09-13-tombstones-00-overview.md`. Read both first.

## Global Constraints

- Slices 1–6 are merged to `main` before this slice starts. Branch from an up-to-date `main`.
- Verification gate before every push and PR, matching CI exactly. Run the commands bare, one per line, never joined with `&&`:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo build --workspace --all-targets --all-features`
  - `cargo test --workspace --all-features`
- Always open a PR and merge it after CI is green. Never push to `main`.
- Commit messages end with `Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`. PR bodies end with `https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`.
- The PR body says `Part of #203` (slice 6 closed #203).
- Contract names are used exactly as the overview defines them: `Record::is_tombstone`, `tombstone_of`, `DEFAULT_ANCESTOR_CAP`, `Store::get_raw`, `Store::list_raw`, `Store::put_raw`, `Store::purge`, `Store::delete_as` (required), `Store::delete` (provided, `delete_as(key, expected, None)`), `gonzalo_core::reset::{reset, ResetReport}`, `gonzalo_core::collect::{collect, CollectReport}`.
- Reconciled contract (wins over any older text in slices 1–6): `put_raw` is the replication write and never re-stamps the **revision**. The author rule is separate: through the daemon, `put_raw` from a non-admin principal restamps `meta.author` to that principal, like consumer `put`, while an admin principal (or open mode) keeps the replicated record's author. A consumer `put` or `put_raw` that the store rejects with `NotFound` returns HTTP 412 / gRPC `FailedPrecondition`, which `ServerStore` maps back to `CoreError::NotFound`. 404 is not used, because on raw routes 404 means "old daemon". A consumer `put` over a tombstone is a recreation with `expected = None` and `NotFound` with any `Some`. `delete_as` takes an optional author, which the daemon stamps from the authenticated principal. On a lost conditional write — `PreconditionFailed` (412), `ConditionalRequestConflict` (409), or `NoSuchKey` (the object vanished to a concurrent purge) — s3 re-reads and re-plans, up to 8 attempts, then returns a `Backend` error. `gonzalod`'s cap is only the env var `GONZALO_ANCESTOR_CAP`.
- CLI synopsis is copied from slice 6's plan: every command takes `--root`; `--expected` is revision JSON as `gonzalo get` prints it; `--older-than` accepts `Nd`/`Nh`/`Nm`/`Ns` and rejects 0; `--ancestor-cap` is per command on `delete`/`reset`/`collect`/`sync`; the CLI opens only a local `FsStore`. Exit codes: **0 success, 1 error, 2 usage, 3 conflict** (`delete`, `reset`). `collect` exits 0 even with conflicts. `gonzalo delete` attributes tombstones to `gonzalo-cli`, and `reset` tombstones carry no author. The exact synopsis and outputs are the table in Task 6's Interfaces.
- Soak test doubles implement `delete_as` and `put_raw`, never `delete`, which is a provided method.
- Tombstone hash domain string, verbatim: `gonzalo:tombstone:v1`. Default ancestor cap: `32`. `deleted_at` is ms since the Unix epoch.
- Never give `get_raw` / `list_raw` / `purge` a default implementation, and never fall back from a raw read to a consumer read, not even in soak test doubles.
- Workspace lints: `unsafe_code = "forbid"`, clippy `all = warn`, promoted to errors by `-D warnings`.
- ADRs are append-only. ADR 0018's **body is not edited.** Only its README index row gains the partial-supersession back-reference.
- ADR header format matches 0018/0020: `- **Status:** accepted` and `- **Date:** 2026-09-13` bullets, then `## Context`, `## Decision`, `## Consequences`.

## What "replicas agree" means in this soak (read before Task 1)

The HA soak runs N `gonzalod` processes that all front **one S3 bucket**. No replica holds its own copy, so none can diverge in storage, and `sync` never runs in the soak. The spec's §6.7 invariant ("every key is live on all replicas or a tombstone on all replicas") therefore can't catch a replication bug here. What it does catch in this topology:

- **Read-path divergence between daemons:** a replica that caches, a replica that misroutes raw reads to consumer reads, or a respawned replica serving stale state.
- **Consumer/raw disagreement:** a replica whose `get` shows a record its own `get_raw` calls a tombstone, or hides one its raw read calls live. This is the tombstone filtering over HTTP.
- **Physical removal:** a key that had committed lifecycle writes but reads as absent. `delete` must never physically remove a record.
- **Resurrection under churn:** an acked delete of a unique key that reads as live, or not as a tombstone, after a replica kill.
- **Conflict arbitration for deletes:** deletes racing edits must produce `DeleteResult::Conflict`. Zero such conflicts means the race was never exercised.

Every soak write is a **consumer** write: `put` for edits and recreations, and `delete` for deletes. A stale conditional edit over a tombstone returns `NotFound`, which the workload records as a lost race. The soak has no replication step, so it never models a replication write and never calls `put_raw`. The `put_raw` resurrection window it closes (a tombstone landing between sync's raw read and its copy) is covered by slice 5's sync tests and the conformance suite, not here.

The oracle is topology-agnostic. It compares per-replica views and would work unchanged over independent stores that had been synced. Cross-store replication itself is covered by the sync and git-pull tests from slice 5 (§6.2, §6.3), not by this soak. The module docs say this so nobody overclaims.

**What needs S3:** only `tests/ha_soak.rs` (the bounded gate) and the `gonzalo-soak` binary. Both need RustFS and a built `gonzalod`. The gate **skips** when `GONZALO_S3_TEST_ENDPOINT` etc. are unset, so `cargo test --workspace` stays green without docker. The `ha-soak` workflow runs it on PRs that touch its paths, and nightly. The oracle unit tests, the dispatcher unit tests, and `workload::tests::in_process_fs_replicas_hold_the_invariant` (three `FsStore` handles over one temp dir, the same shared-storage topology) run in the normal `ci` job with no S3.

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/gonzalo-soak/src/oracle.rs` | Modify | New lifecycle types, violations and checks; unit tests |
| `crates/gonzalo-soak/src/dispatch.rs` | Modify | `Dispatcher::delete`, `Dispatcher::get_raw`, `Dispatcher::replicas`; tests |
| `crates/gonzalo-soak/src/workload.rs` | Replace | Lifecycle stream, unique deletes, `collect_lifecycle`; tests |
| `crates/gonzalo-soak/src/harness.rs` | Modify | Collect per-replica lifecycle state after respawn |
| `crates/gonzalo-soak/src/main.rs` | Modify | New flags, delete-conflict reporting |
| `crates/gonzalo-soak/tests/ha_soak.rs` | Modify | Lifecycle config in the bounded gate |
| `.github/workflows/ha-soak.yml` | Modify | Trigger on `gonzalo-core` / `gonzalo-store-server` changes |
| `docs/adr/0021-replicated-deletion-with-tombstones.md` | Create | The decision record |
| `docs/adr/README.md` | Modify | 0018 row back-reference, new 0021 row |
| `docs/guide/src/deletion.md` | Create | User guide: delete / reset / collect / horizon / upgrade |
| `docs/guide/src/SUMMARY.md` | Modify | Guide entry (and the regenerated ADR block) |
| `docs/evaluation/competitors/mem0/parity-gap-matrix.md` | Modify | Delete row cites ADR 0021 |
| `CHANGELOG.md` | Modify | `[Unreleased]` entries and upgrade warning |

---

### Task 1: Oracle lifecycle invariants

**Files:**
- Modify: `crates/gonzalo-soak/src/oracle.rs` (whole file)
- Modify: `crates/gonzalo-soak/src/workload.rs:82-89` and `:207-211` (compile fix only: new struct fields)

**Interfaces:**
- Consumes: nothing new (the oracle stays pure and doesn't depend on `gonzalo-core`).
- Produces (used by Tasks 3–4):
  - `pub enum LifecycleOp { Edit, Delete, Recreate }` (`Copy`, `Eq`)
  - `pub enum LifecycleResult { Committed, Conflict, Skipped, Failed }` (`Copy`, `Eq`)
  - `pub struct LifecycleRecord { pub key: String, pub op_id: u64, pub op: LifecycleOp, pub result: LifecycleResult }`
  - `pub enum RawState { Absent, Live { revision: String }, Tombstone { revision: String }, ReadFailed { error: String } }`
  - `pub struct ReplicaView { pub raw: RawState, pub consumer_live: Option<bool> }`
  - `pub struct FinalLifecycle { pub key: String, pub replicas: Vec<ReplicaView> }`
  - `FinalUnique` gains `pub deleted: bool, pub raw_tombstone: bool`
  - `SoakStats` gains `pub lifecycle_ops: Vec<LifecycleRecord>, pub lifecycle: Vec<FinalLifecycle>`
  - `pub fn delete_conflicts(stats: &SoakStats) -> usize`
  - New `Violation` variants: `MixedLiveAndTombstone { key, states: Vec<RawState> }`, `ReplicasDisagree { key, states: Vec<RawState> }`, `ConsumerRawMismatch { key, replica: usize }`, `FinalReadFailed { key, replica: usize }`, `LifecycleKeyVanished { key }`, `NoDeleteConflictsObserved`, `NoDeletesCommitted`, `LifecycleNotChecked`, `AckedDeleteLost { key }`

- [ ] **Step 0: Branch from main**

```bash
git checkout main
git pull --ff-only
git checkout -b feat/203-tombstones-07-soak-and-docs
```

Expected: `Switched to a new branch 'feat/203-tombstones-07-soak-and-docs'`.

- [ ] **Step 1: Write the failing tests**

In `crates/gonzalo-soak/src/oracle.rs`, replace the entire `#[cfg(test)] mod tests { ... }` block (from `#[cfg(test)]` to the end of the file) with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn live(rev: &str) -> ReplicaView {
        ReplicaView {
            raw: RawState::Live {
                revision: rev.into(),
            },
            consumer_live: Some(true),
        }
    }

    fn tomb(rev: &str) -> ReplicaView {
        ReplicaView {
            raw: RawState::Tombstone {
                revision: rev.into(),
            },
            consumer_live: Some(false),
        }
    }

    fn lop(key: &str, op_id: u64, op: LifecycleOp, result: LifecycleResult) -> LifecycleRecord {
        LifecycleRecord {
            key: key.into(),
            op_id,
            op,
            result,
        }
    }

    /// A clean run: two contended keys whose final sets hold exactly their
    /// committed op-ids, conflicts were seen, unique writes stuck, the acked
    /// unique delete is still a tombstone, both lifecycle keys agree across all
    /// three replicas, a delete conflict was observed, and all writers finished.
    /// Must report zero violations.
    fn clean_stats() -> SoakStats {
        SoakStats {
            ops: vec![
                OpRecord {
                    key: "k1".into(),
                    op_id: 1,
                    result: OpResult::Committed,
                },
                OpRecord {
                    key: "k1".into(),
                    op_id: 2,
                    result: OpResult::Committed,
                },
                OpRecord {
                    key: "k1".into(),
                    op_id: 3,
                    result: OpResult::Conflict,
                },
                OpRecord {
                    key: "k2".into(),
                    op_id: 4,
                    result: OpResult::Committed,
                },
            ],
            contended: vec![
                FinalContended {
                    key: "k1".into(),
                    members: vec![1, 2],
                },
                FinalContended {
                    key: "k2".into(),
                    members: vec![4],
                },
            ],
            unique: vec![
                FinalUnique {
                    key: "u1".into(),
                    acked: true,
                    readable_with_value: true,
                    deleted: false,
                    raw_tombstone: false,
                },
                FinalUnique {
                    key: "u2".into(),
                    acked: true,
                    readable_with_value: false,
                    deleted: true,
                    raw_tombstone: true,
                },
            ],
            conflicts_observed: 3,
            writers_completed: 4,
            writers_total: 4,
            lifecycle_ops: vec![
                lop("life-0", 0, LifecycleOp::Recreate, LifecycleResult::Committed),
                lop("life-0", 1, LifecycleOp::Delete, LifecycleResult::Committed),
                lop("life-0", 2, LifecycleOp::Delete, LifecycleResult::Conflict),
                lop("life-1", 3, LifecycleOp::Recreate, LifecycleResult::Committed),
                lop("life-1", 4, LifecycleOp::Edit, LifecycleResult::Skipped),
            ],
            lifecycle: vec![
                FinalLifecycle {
                    key: "life-0".into(),
                    replicas: vec![tomb("1:t"), tomb("1:t"), tomb("1:t")],
                },
                FinalLifecycle {
                    key: "life-1".into(),
                    replicas: vec![live("0:h"), live("0:h"), live("0:h")],
                },
            ],
        }
    }

    #[test]
    fn clean_run_has_no_violations() {
        assert_eq!(check(&clean_stats()), vec![]);
    }

    #[test]
    fn detects_lost_update() {
        let mut s = clean_stats();
        // op-id 2 committed but is missing from k1's final set — a lost update.
        s.contended[0].members = vec![1];
        let v = check(&s);
        assert!(
            v.contains(&Violation::LostUpdate {
                key: "k1".into(),
                missing: vec![2]
            }),
            "expected LostUpdate, got {v:?}"
        );
    }

    #[test]
    fn detects_no_conflicts_observed() {
        let mut s = clean_stats();
        s.conflicts_observed = 0; // the race was never actually exercised
        assert!(
            check(&s).contains(&Violation::NoConflictsObserved),
            "zero observed conflicts must be flagged"
        );
    }

    #[test]
    fn detects_unique_write_lost() {
        let mut s = clean_stats();
        s.unique[0].readable_with_value = false;
        assert!(
            check(&s).contains(&Violation::UniqueWriteLost { key: "u1".into() }),
            "an acked unique write that isn't readable is a lost write"
        );
    }

    #[test]
    fn deleted_unique_key_is_not_a_lost_write() {
        // u2 is unreadable *because* it was deleted — that is not UniqueWriteLost.
        let v = check(&clean_stats());
        assert!(
            !v.contains(&Violation::UniqueWriteLost { key: "u2".into() }),
            "a deleted unique key must not be reported as a lost write: {v:?}"
        );
    }

    #[test]
    fn detects_acked_delete_lost_when_readable_again() {
        let mut s = clean_stats();
        s.unique[1].readable_with_value = true; // resurrected
        assert!(check(&s).contains(&Violation::AckedDeleteLost { key: "u2".into() }));
    }

    #[test]
    fn detects_acked_delete_lost_when_not_a_tombstone() {
        let mut s = clean_stats();
        s.unique[1].raw_tombstone = false; // physically removed or never written
        assert!(check(&s).contains(&Violation::AckedDeleteLost { key: "u2".into() }));
    }

    #[test]
    fn detects_mixed_live_and_tombstone() {
        let mut s = clean_stats();
        s.lifecycle[0].replicas[2] = live("2:h");
        let v = check(&s);
        assert!(
            v.iter().any(|x| matches!(
                x,
                Violation::MixedLiveAndTombstone { key, .. } if key == "life-0"
            )),
            "live on one replica and tombstone on others must be flagged: {v:?}"
        );
        assert!(
            !v.iter()
                .any(|x| matches!(x, Violation::ReplicasDisagree { .. })),
            "a mixed key is reported once, as MixedLiveAndTombstone: {v:?}"
        );
    }

    #[test]
    fn detects_replicas_disagree_on_revision() {
        let mut s = clean_stats();
        s.lifecycle[0].replicas[1] = tomb("2:t");
        let v = check(&s);
        assert!(
            v.iter().any(|x| matches!(
                x,
                Violation::ReplicasDisagree { key, .. } if key == "life-0"
            )),
            "two tombstones at different revisions must be flagged: {v:?}"
        );
    }

    #[test]
    fn detects_absent_on_one_replica_as_disagreement_and_vanished() {
        let mut s = clean_stats();
        s.lifecycle[1].replicas[0] = ReplicaView {
            raw: RawState::Absent,
            consumer_live: Some(false),
        };
        let v = check(&s);
        assert!(
            v.iter().any(|x| matches!(
                x,
                Violation::ReplicasDisagree { key, .. } if key == "life-1"
            )),
            "{v:?}"
        );
        assert!(v.contains(&Violation::LifecycleKeyVanished {
            key: "life-1".into()
        }));
    }

    #[test]
    fn detects_lifecycle_key_vanished_everywhere() {
        let mut s = clean_stats();
        for r in &mut s.lifecycle[0].replicas {
            *r = ReplicaView {
                raw: RawState::Absent,
                consumer_live: Some(false),
            };
        }
        let v = check(&s);
        assert!(
            v.contains(&Violation::LifecycleKeyVanished {
                key: "life-0".into()
            }),
            "a committed-then-deleted key must be a tombstone, never physically gone: {v:?}"
        );
    }

    #[test]
    fn untouched_absent_key_is_not_vanished() {
        let mut s = clean_stats();
        s.lifecycle.push(FinalLifecycle {
            key: "life-2".into(),
            replicas: vec![
                ReplicaView {
                    raw: RawState::Absent,
                    consumer_live: Some(false),
                };
                3
            ],
        });
        assert_eq!(check(&s), vec![], "no committed op on life-2, so absent is fine");
    }

    #[test]
    fn detects_consumer_raw_mismatch() {
        let mut s = clean_stats();
        // Replica 1 serves a tombstoned key to consumers.
        s.lifecycle[0].replicas[1].consumer_live = Some(true);
        assert!(check(&s).contains(&Violation::ConsumerRawMismatch {
            key: "life-0".into(),
            replica: 1
        }));
    }

    #[test]
    fn detects_final_read_failed() {
        let mut s = clean_stats();
        s.lifecycle[1].replicas[2] = ReplicaView {
            raw: RawState::ReadFailed {
                error: "connection refused".into(),
            },
            consumer_live: Some(true),
        };
        s.lifecycle[0].replicas[0].consumer_live = None;
        let v = check(&s);
        assert!(v.contains(&Violation::FinalReadFailed {
            key: "life-1".into(),
            replica: 2
        }));
        assert!(v.contains(&Violation::FinalReadFailed {
            key: "life-0".into(),
            replica: 0
        }));
        assert!(
            !v.iter()
                .any(|x| matches!(x, Violation::ReplicasDisagree { .. })),
            "an unreadable replica is FinalReadFailed, not a disagreement: {v:?}"
        );
    }

    #[test]
    fn detects_no_delete_conflicts_observed() {
        let mut s = clean_stats();
        s.lifecycle_ops
            .retain(|o| !(o.op == LifecycleOp::Delete && o.result == LifecycleResult::Conflict));
        assert_eq!(delete_conflicts(&s), 0);
        assert!(check(&s).contains(&Violation::NoDeleteConflictsObserved));
    }

    #[test]
    fn edit_conflicts_do_not_count_as_delete_conflicts() {
        let mut s = clean_stats();
        s.lifecycle_ops
            .retain(|o| !(o.op == LifecycleOp::Delete && o.result == LifecycleResult::Conflict));
        s.lifecycle_ops.push(lop(
            "life-1",
            9,
            LifecycleOp::Edit,
            LifecycleResult::Conflict,
        ));
        assert!(check(&s).contains(&Violation::NoDeleteConflictsObserved));
    }

    #[test]
    fn detects_no_deletes_committed() {
        let mut s = clean_stats();
        s.lifecycle_ops
            .retain(|o| !(o.op == LifecycleOp::Delete && o.result == LifecycleResult::Committed));
        assert!(check(&s).contains(&Violation::NoDeletesCommitted));
    }

    #[test]
    fn detects_lifecycle_not_checked() {
        let mut s = clean_stats();
        s.lifecycle.clear(); // the harness forgot to read every replica
        assert!(check(&s).contains(&Violation::LifecycleNotChecked));
    }

    #[test]
    fn run_without_lifecycle_ops_skips_lifecycle_checks() {
        let mut s = clean_stats();
        s.lifecycle_ops.clear();
        s.lifecycle.clear();
        assert_eq!(check(&s), vec![]);
    }

    #[test]
    fn detects_writers_did_not_complete() {
        let mut s = clean_stats();
        s.writers_completed = 3;
        assert!(check(&s).contains(&Violation::WritersDidNotComplete {
            completed: 3,
            total: 4
        }));
    }

    #[test]
    fn detects_no_progress() {
        let s = SoakStats {
            ops: vec![OpRecord {
                key: "k1".into(),
                op_id: 1,
                result: OpResult::Failed,
            }],
            contended: vec![FinalContended {
                key: "k1".into(),
                members: vec![],
            }],
            writers_total: 4,
            ..Default::default()
        };
        assert!(check(&s).contains(&Violation::NoProgress));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-soak --lib oracle`
Expected: compile FAIL, e.g. `error[E0412]: cannot find type LifecycleRecord in this scope` and `struct FinalUnique has no field named deleted`.

- [ ] **Step 3: Implement the oracle**

Replace everything in `crates/gonzalo-soak/src/oracle.rs` **above** `#[cfg(test)]` with:

```rust
//! The soak safety/liveness oracle.
//!
//! Given the recorded outcome of every write op plus a final read of every key,
//! [`check`] asserts the invariants a correct conditional-write store must hold
//! under concurrent multi-replica load and replica-kill chaos:
//!
//! - **No lost update** — every *committed* op-id on a contended key survives in
//!   that key's final record, exactly once, and the revision chain grew by one
//!   per committed put.
//! - **Conflicts surface** — racing writers observed `Conflict` (never a silent
//!   overwrite). Zero observed conflicts means the invariant was never actually
//!   exercised, which is itself a failure. The same holds for deletes racing
//!   edits: zero delete conflicts is a failure.
//! - **Durability under churn** — every acked unique-key write is still readable,
//!   and every acked delete still reads as a tombstone (no resurrection).
//! - **Replicas agree on deletion** (#203) — once the writers have stopped and every
//!   replica is live again, each lifecycle key is live on every replica or a
//!   tombstone on every replica (never a mix), every replica reports the same raw
//!   revision, each replica's consumer read agrees with its own raw read, and a
//!   key that had committed writes is never physically absent.
//! - **Liveness** — the run made progress and every writer finished.
//!
//! **What "replicas agree" means here.** The HA soak's replicas are `gonzalod`
//! processes over *one shared bucket*, so no replica holds its own copy. In that
//! topology the agreement check catches daemon read-path bugs (caching, raw reads
//! routed to consumer reads, tombstone filtering), not storage replication, which
//! the sync and git-pull tests cover. The check itself is topology-agnostic: it
//! compares per-replica views and would apply unchanged to independent stores
//! that had been synced.
//!
//! This is a targeted invariant oracle, not a linearizability checker: it asserts
//! on set membership / agreement / completion, never on exact interleavings,
//! so normal scheduling jitter cannot flake it.

use std::collections::BTreeSet;

/// The result of a single conditional-write op, as observed by the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpResult {
    /// The conditional put committed.
    Committed,
    /// The put was rejected as a conflict — a concurrent writer won the race.
    Conflict,
    /// The op ultimately failed (transport error, exhausted retries).
    Failed,
}

/// One recorded write against a contended key: the unique op-id and its result.
#[derive(Debug, Clone)]
pub struct OpRecord {
    pub key: String,
    pub op_id: u64,
    pub result: OpResult,
}

/// The final observed state of one contended key.
#[derive(Debug, Clone)]
pub struct FinalContended {
    pub key: String,
    /// The op-ids present in the final record's accumulated set (read from storage).
    pub members: Vec<u64>,
}

/// The final observed state of one unique (uncontended) key.
#[derive(Debug, Clone)]
pub struct FinalUnique {
    pub key: String,
    /// The write was acked (`Committed`) by the driver.
    pub acked: bool,
    /// The key is readable (consumer `get`) with the exact value that was written.
    pub readable_with_value: bool,
    /// The driver later deleted this key and the delete was acked (`Deleted`).
    pub deleted: bool,
    /// A raw read returned a tombstone. Only meaningful when `deleted`.
    pub raw_tombstone: bool,
}

/// A lifecycle op against a `life-*` key: the delete/recreate stream (#203).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleOp {
    /// Conditionally update a live record. Skipped when the key reads as absent.
    Edit,
    /// Conditionally tombstone a live record. Skipped when the key reads as absent.
    Delete,
    /// Create the key when it reads as absent. Over a tombstone this is a
    /// recreation. Skipped when the key is live.
    Recreate,
}

/// The outcome of one lifecycle op. Lifecycle ops are not retried: a lost race
/// is recorded as `Conflict` and the writer moves on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleResult {
    /// The conditional put or delete committed.
    Committed,
    /// A concurrent writer moved the key between the read and the write.
    Conflict,
    /// The op did not apply to the key's state (e.g. delete of an absent key).
    Skipped,
    /// A transport or backend error.
    Failed,
}

/// One recorded lifecycle op.
#[derive(Debug, Clone)]
pub struct LifecycleRecord {
    pub key: String,
    pub op_id: u64,
    pub op: LifecycleOp,
    pub result: LifecycleResult,
}

/// What one replica's raw read (`get_raw`) returned for a key after settling.
/// Revisions are rendered as `"<counter>:<hash>"` so the oracle stays pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawState {
    Absent,
    Live { revision: String },
    Tombstone { revision: String },
    ReadFailed { error: String },
}

/// One replica's view of a lifecycle key after settling.
#[derive(Debug, Clone)]
pub struct ReplicaView {
    /// The replica's raw read.
    pub raw: RawState,
    /// Whether the replica's consumer read (`get`) returned a record; `None` if
    /// that read failed.
    pub consumer_live: Option<bool>,
}

/// The final per-replica state of one lifecycle key, in replica order.
#[derive(Debug, Clone)]
pub struct FinalLifecycle {
    pub key: String,
    pub replicas: Vec<ReplicaView>,
}

/// Everything the oracle needs: op outcomes, final reads, and writer completion.
#[derive(Debug, Clone, Default)]
pub struct SoakStats {
    pub ops: Vec<OpRecord>,
    pub contended: Vec<FinalContended>,
    pub unique: Vec<FinalUnique>,
    /// Total transient `Conflict` outcomes observed across all RMW retries — the
    /// evidence the CAS actually arbitrated racing writers. Zero means the race
    /// invariant was never exercised.
    pub conflicts_observed: u64,
    pub writers_completed: usize,
    pub writers_total: usize,
    /// Every lifecycle op and its outcome.
    pub lifecycle_ops: Vec<LifecycleRecord>,
    /// Per-replica final state of every lifecycle key. Filled by
    /// `workload::collect_lifecycle` once every replica is live again.
    pub lifecycle: Vec<FinalLifecycle>,
}

/// A violated invariant. An empty [`check`] result means the soak passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// Committed op-ids missing from the contended key's final set — a lost update.
    LostUpdate { key: String, missing: Vec<u64> },
    /// An op-id appears more than once in a contended key's final set.
    DuplicateUpdate { key: String, duplicated: Vec<u64> },
    /// No conflicts were observed anywhere — the race invariant was not exercised.
    NoConflictsObserved,
    /// An acked unique-key write is missing or has the wrong value after chaos.
    UniqueWriteLost { key: String },
    /// Not every writer task completed within the deadline (liveness).
    WritersDidNotComplete { completed: usize, total: usize },
    /// The run committed nothing at all (liveness).
    NoProgress,
    /// A lifecycle key is live on some replicas and a tombstone on others.
    MixedLiveAndTombstone { key: String, states: Vec<RawState> },
    /// Replicas that read successfully disagree on a lifecycle key's raw state
    /// (different revisions, or absent on some).
    ReplicasDisagree { key: String, states: Vec<RawState> },
    /// A replica's consumer read disagrees with its own raw read: a tombstone
    /// served to consumers, or a live record hidden from them.
    ConsumerRawMismatch { key: String, replica: usize },
    /// A replica could not be read after settling.
    FinalReadFailed { key: String, replica: usize },
    /// A lifecycle key with committed writes reads as absent — delete must write
    /// a tombstone, never physically remove.
    LifecycleKeyVanished { key: String },
    /// Lifecycle ops ran but no delete ever lost a race — the delete-conflict
    /// invariant was not exercised.
    NoDeleteConflictsObserved,
    /// Lifecycle ops ran but no delete ever committed.
    NoDeletesCommitted,
    /// Lifecycle ops ran but no per-replica final state was collected.
    LifecycleNotChecked,
    /// An acked delete of a unique key reads as live again, or not as a tombstone.
    AckedDeleteLost { key: String },
}

/// Delete ops that lost a race (`Conflict`). Edit and recreate conflicts don't count.
pub fn delete_conflicts(stats: &SoakStats) -> usize {
    stats
        .lifecycle_ops
        .iter()
        .filter(|o| o.op == LifecycleOp::Delete && o.result == LifecycleResult::Conflict)
        .count()
}

/// Check every soak invariant. Returns the (possibly empty) set of violations.
pub fn check(stats: &SoakStats) -> Vec<Violation> {
    let mut out = Vec::new();

    // Per contended key: committed op-ids must all survive, exactly once.
    for fc in &stats.contended {
        let committed: Vec<u64> = stats
            .ops
            .iter()
            .filter(|o| o.key == fc.key && o.result == OpResult::Committed)
            .map(|o| o.op_id)
            .collect();

        let missing: Vec<u64> = committed
            .iter()
            .copied()
            .filter(|id| !fc.members.contains(id))
            .collect();
        if !missing.is_empty() {
            out.push(Violation::LostUpdate {
                key: fc.key.clone(),
                missing,
            });
        }

        let duplicated: Vec<u64> = fc
            .members
            .iter()
            .copied()
            .filter(|id| fc.members.iter().filter(|m| *m == id).count() > 1)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !duplicated.is_empty() {
            out.push(Violation::DuplicateUpdate {
                key: fc.key.clone(),
                duplicated,
            });
        }
    }

    // The race invariant must actually have been exercised: with real contention
    // across replicas, some RMW attempts must lose the CAS and observe `Conflict`.
    if stats.conflicts_observed == 0 {
        out.push(Violation::NoConflictsObserved);
    }

    // Durability under churn: every acked unique write must still be readable,
    // and every acked delete must still be a tombstone.
    for fu in &stats.unique {
        if fu.acked && !fu.deleted && !fu.readable_with_value {
            out.push(Violation::UniqueWriteLost {
                key: fu.key.clone(),
            });
        }
        if fu.deleted && (fu.readable_with_value || !fu.raw_tombstone) {
            out.push(Violation::AckedDeleteLost {
                key: fu.key.clone(),
            });
        }
    }

    check_lifecycle(stats, &mut out);

    // Liveness: the run made progress and every writer finished.
    let committed_total = stats
        .ops
        .iter()
        .filter(|o| o.result == OpResult::Committed)
        .count();
    if committed_total == 0 {
        out.push(Violation::NoProgress);
    }
    if stats.writers_completed < stats.writers_total {
        out.push(Violation::WritersDidNotComplete {
            completed: stats.writers_completed,
            total: stats.writers_total,
        });
    }

    out
}

/// The #203 deletion invariants over the lifecycle stream. A run with no
/// lifecycle ops (e.g. `--lifecycle-ops-per-writer 0`) skips them entirely.
fn check_lifecycle(stats: &SoakStats, out: &mut Vec<Violation>) {
    if stats.lifecycle_ops.is_empty() {
        return;
    }
    if stats.lifecycle.is_empty() {
        out.push(Violation::LifecycleNotChecked);
    }
    if delete_conflicts(stats) == 0 {
        out.push(Violation::NoDeleteConflictsObserved);
    }
    if !stats
        .lifecycle_ops
        .iter()
        .any(|o| o.op == LifecycleOp::Delete && o.result == LifecycleResult::Committed)
    {
        out.push(Violation::NoDeletesCommitted);
    }

    let written: BTreeSet<&str> = stats
        .lifecycle_ops
        .iter()
        .filter(|o| o.result == LifecycleResult::Committed)
        .map(|o| o.key.as_str())
        .collect();

    for fl in &stats.lifecycle {
        for (replica, view) in fl.replicas.iter().enumerate() {
            match (&view.raw, view.consumer_live) {
                (RawState::ReadFailed { .. }, _) | (_, None) => {
                    out.push(Violation::FinalReadFailed {
                        key: fl.key.clone(),
                        replica,
                    });
                }
                (raw, Some(consumer_live)) => {
                    let raw_live = matches!(raw, RawState::Live { .. });
                    if consumer_live != raw_live {
                        out.push(Violation::ConsumerRawMismatch {
                            key: fl.key.clone(),
                            replica,
                        });
                    }
                }
            }
        }

        let states: Vec<RawState> = fl.replicas.iter().map(|v| v.raw.clone()).collect();
        let readable: Vec<&RawState> = states
            .iter()
            .filter(|s| !matches!(s, RawState::ReadFailed { .. }))
            .collect();
        let any_live = readable.iter().any(|s| matches!(s, RawState::Live { .. }));
        let any_tomb = readable
            .iter()
            .any(|s| matches!(s, RawState::Tombstone { .. }));
        let any_absent = readable.iter().any(|s| matches!(s, RawState::Absent));

        if any_live && any_tomb {
            out.push(Violation::MixedLiveAndTombstone {
                key: fl.key.clone(),
                states: states.clone(),
            });
        } else if readable.windows(2).any(|w| w[0] != w[1]) {
            out.push(Violation::ReplicasDisagree {
                key: fl.key.clone(),
                states: states.clone(),
            });
        }

        if any_absent && written.contains(fl.key.as_str()) {
            out.push(Violation::LifecycleKeyVanished {
                key: fl.key.clone(),
            });
        }
    }
}
```

- [ ] **Step 4: Keep `workload.rs` compiling with the new fields**

In `crates/gonzalo-soak/src/workload.rs`, inside `run`, replace the `SoakStats { ... }` literal with:

```rust
    SoakStats {
        ops,
        contended,
        unique,
        conflicts_observed,
        writers_completed,
        writers_total: cfg.writers,
        lifecycle_ops: Vec::new(),
        lifecycle: Vec::new(),
    }
```

and inside `collect_unique`, replace the `out.push(FinalUnique { ... });` with:

```rust
        out.push(FinalUnique {
            key: key_id.clone(),
            acked: true,
            readable_with_value,
            deleted: false,
            raw_tombstone: false,
        });
```

(Task 3 replaces this file wholesale. This step only keeps the crate compiling between commits.)

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-soak --lib oracle`
Expected: PASS, `test result: ok. 21 passed; 0 failed`.

Run: `cargo test -p gonzalo-soak --lib`
Expected: PASS. The existing workload and dispatch tests still pass, because lifecycle checks are skipped when there are no lifecycle ops.

- [ ] **Step 6: Commit**

```bash
git add crates/gonzalo-soak/src/oracle.rs crates/gonzalo-soak/src/workload.rs
git commit -m "test(soak): oracle invariants for replicated deletion

Replicas must agree on live-vs-tombstone and revision per key, consumer
reads must match raw reads, deleted keys must never vanish, and delete
conflicts must actually be exercised (#203).

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 2: Dispatcher delete, raw read, and replica access

**Files:**
- Modify: `crates/gonzalo-soak/src/dispatch.rs`

**Interfaces:**
- Consumes: `Store::delete` (provided), `Store::delete_as`, `Store::put_raw`, `Store::get_raw` (slice 1 contract), `DeleteResult`.
- Produces (used by Tasks 3–4):
  - `pub fn replicas(&self) -> &[Arc<dyn Store>]`
  - `pub async fn delete(&self, key: &RecordKey, expected: Option<Revision>) -> Result<DeleteResult>`
  - `pub async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>>`

- [ ] **Step 1: Write the failing tests**

In `crates/gonzalo-soak/src/dispatch.rs` tests module, make `MockStore`'s `get_raw` honour liveness and count calls. Slice 1 gave it an interim body. It also moved `MockStore`'s liveness-aware `delete` body into `delete_as`, which the provided `delete` calls, so the delete failover test below counts calls through it. If `MockStore` still implements `delete` directly, rename that method to `delete_as` with an extra `_author: Option<Identity>` parameter. Replace the whole `get_raw` method inside `impl Store for MockStore` with:

```rust
        async fn get_raw(&self, _key: &RecordKey) -> Result<Option<Record>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.alive.load(Ordering::SeqCst) {
                Ok(None)
            } else {
                Err(CoreError::Backend("connection refused".into()))
            }
        }
```

Then append these tests at the end of the tests module, before its closing `}`:

```rust
    #[tokio::test]
    async fn delete_fails_over_too() {
        let dead = MockStore::dead();
        let live = MockStore::alive();
        let d = Dispatcher::new(vec![as_store(&dead), as_store(&live)]);
        let out = d.delete(&RecordKey::new("ns", "col", "k"), None).await;
        assert!(
            matches!(out, Ok(DeleteResult::Deleted)),
            "delete failed over: {out:?}"
        );
        assert_eq!(dead.calls(), 1, "dead replica was tried");
        assert_eq!(live.calls(), 1, "then the live replica served it");
    }

    #[tokio::test]
    async fn get_raw_fails_over_too() {
        let dead = MockStore::dead();
        let live = MockStore::alive();
        let d = Dispatcher::new(vec![as_store(&dead), as_store(&live)]);
        let out = d.get_raw(&RecordKey::new("ns", "col", "k")).await;
        assert!(matches!(out, Ok(None)), "get_raw failed over: {out:?}");
        assert_eq!(dead.calls() + live.calls(), 2);
    }

    #[tokio::test]
    async fn replicas_exposes_every_handle_in_order() {
        let a = MockStore::alive();
        let b = MockStore::dead();
        let d = Dispatcher::new(vec![as_store(&a), as_store(&b)]);
        assert_eq!(d.replicas().len(), 2);
        // Reading each handle directly bypasses round-robin and failover.
        assert!(d.replicas()[0].get_raw(&RecordKey::new("ns", "col", "k")).await.is_ok());
        assert!(d.replicas()[1].get_raw(&RecordKey::new("ns", "col", "k")).await.is_err());
    }

    // A delete conflict from a live replica is a definitive answer, never retried.
    #[tokio::test]
    async fn delete_conflict_is_returned_not_retried() {
        struct DeleteConflicter(AtomicUsize);
        #[async_trait]
        impl Store for DeleteConflicter {
            async fn get(&self, _k: &RecordKey) -> Result<Option<Record>> {
                Ok(None)
            }
            async fn put(&self, _r: Record, _e: Option<Revision>) -> Result<PutResult> {
                Ok(PutResult::Committed(Revision::initial(b"x")))
            }
            async fn list(&self, _p: &KeyPrefix) -> Result<Vec<RecordKey>> {
                Ok(Vec::new())
            }
            // `delete` is a provided method that calls `delete_as(key, expected, None)`.
            async fn delete_as(
                &self,
                _k: &RecordKey,
                _e: Option<Revision>,
                _author: Option<Identity>,
            ) -> Result<DeleteResult> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(DeleteResult::Conflict(Box::new(Conflict {
                    key: RecordKey::new("ns", "col", "k"),
                    expected: Some(Revision::initial(b"stale")),
                    current: rec(),
                })))
            }
            async fn get_raw(&self, _k: &RecordKey) -> Result<Option<Record>> {
                Ok(None)
            }
            async fn put_raw(&self, _r: Record, _e: Option<Revision>) -> Result<PutResult> {
                Ok(PutResult::Committed(Revision::initial(b"x")))
            }
            async fn list_raw(&self, _p: &KeyPrefix) -> Result<Vec<RecordKey>> {
                Ok(Vec::new())
            }
            async fn purge(&self, _k: &RecordKey, _e: Revision) -> Result<DeleteResult> {
                Ok(DeleteResult::Deleted)
            }
        }
        let c0 = Arc::new(DeleteConflicter(AtomicUsize::new(0)));
        let c1 = Arc::new(DeleteConflicter(AtomicUsize::new(0)));
        let d = Dispatcher::new(vec![
            c0.clone() as Arc<dyn Store>,
            c1.clone() as Arc<dyn Store>,
        ]);
        let out = d
            .delete(&RecordKey::new("ns", "col", "k"), Some(Revision::initial(b"stale")))
            .await;
        assert!(
            matches!(out, Ok(DeleteResult::Conflict(_))),
            "conflict returned: {out:?}"
        );
        assert_eq!(
            c0.0.load(Ordering::SeqCst) + c1.0.load(Ordering::SeqCst),
            1,
            "a delete conflict is not retried on another replica"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-soak --lib dispatch`
Expected: compile FAIL with `no method named delete found for struct Dispatcher` (and the same for `get_raw` and `replicas`).

- [ ] **Step 3: Implement**

In `crates/gonzalo-soak/src/dispatch.rs`, change the import line to:

```rust
use gonzalo_core::{DeleteResult, PutResult, Record, RecordKey, Result, Revision, Store};
```

Insert these methods in `impl Dispatcher`, directly after `is_empty`:

```rust
    /// Every replica handle, in construction order. Used to read **each**
    /// replica directly (no round-robin, no failover) once the run has settled.
    pub fn replicas(&self) -> &[Arc<dyn Store>] {
        &self.replicas
    }
```

and these after `put`, inside `impl Dispatcher`:

```rust
    /// `delete` with round-robin start + failover across all replicas. A
    /// `Conflict` is a valid `Ok` outcome and returns immediately (not a failover).
    /// A failover after a delete that did commit on the dead replica is safe: the
    /// retry sees a tombstone and returns `Deleted` without writing.
    pub async fn delete(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
    ) -> Result<DeleteResult> {
        let n = self.replicas.len();
        let start = self.start();
        let mut last_err = None;
        for offset in 0..n {
            let idx = (start + offset) % n;
            match self.replicas[idx].delete(key, expected.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.expect("at least one replica was attempted"))
    }

    /// `get_raw` (tombstones visible) with round-robin start + failover.
    pub async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        let n = self.replicas.len();
        let start = self.start();
        let mut last_err = None;
        for offset in 0..n {
            let idx = (start + offset) % n;
            match self.replicas[idx].get_raw(key).await {
                Ok(v) => return Ok(v),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.expect("at least one replica was attempted"))
    }
```

Also update the module doc's first paragraph to say it fans out `get`, `get_raw`, `put` and `delete`, replacing "round-robins each op to a replica" with "round-robins each op (`get`, `get_raw`, `put`, `delete`) to a replica". Leave the tests module's `use gonzalo_core::{...}` line as it is. Its explicit `DeleteResult` import shadows the one `super::*` now brings in, which compiles and lints cleanly.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-soak --lib dispatch`
Expected: PASS, `test result: ok. 10 passed; 0 failed`.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-soak/src/dispatch.rs
git commit -m "feat(soak): dispatcher delete, get_raw and per-replica access

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 3: Workload lifecycle stream and per-replica collection

**Files:**
- Replace: `crates/gonzalo-soak/src/workload.rs` (whole file)

**Interfaces:**
- Consumes: Task 1 oracle types; Task 2 `Dispatcher::{delete, get_raw, replicas}`; slice 1 `Record { ancestors, deleted_at }`, `Record::is_tombstone`, `gonzalo_core::tombstone_of`, `DEFAULT_ANCESTOR_CAP`.
- Produces (used by Task 4):
  - `WorkloadConfig` gains `pub lifecycle_keys: usize` (default 2), `pub lifecycle_ops_per_writer: usize` (default 20), `pub unique_deletes_per_writer: usize` (default 1)
  - `pub fn lifecycle_op_for(life_id: u64) -> LifecycleOp`
  - `pub fn lifecycle_key_for(life_id: u64, lifecycle_keys: usize) -> String`
  - `pub fn raw_state(read: gonzalo_core::Result<Option<Record>>) -> RawState`
  - `pub async fn collect_lifecycle(replicas: &[Arc<dyn Store>], cfg: &WorkloadConfig) -> Vec<FinalLifecycle>`
  - `pub async fn run(dispatcher: Arc<Dispatcher>, cfg: WorkloadConfig) -> SoakStats` (signature unchanged; `lifecycle` left empty)

- [ ] **Step 1: Write the failing tests**

Replace the whole `#[cfg(test)] mod tests { ... }` block at the end of `crates/gonzalo-soak/src/workload.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::{LifecycleOp, LifecycleResult, RawState};
    use gonzalo_core::{DEFAULT_ANCESTOR_CAP, Store, tombstone_of};
    use gonzalo_store_fs::FsStore;
    use std::collections::BTreeSet;

    #[test]
    fn parse_round_trips() {
        assert_eq!(parse_members(&encode_members(&[3, 1, 2])), vec![3, 1, 2]);
        assert_eq!(parse_members(b""), Vec::<u64>::new());
    }

    /// Key and op choice must be decorrelated: every lifecycle key must see every
    /// op kind, or a key that is never deleted (or never recreated) would make
    /// the deletion invariant vacuous for it.
    #[test]
    fn lifecycle_ops_cover_every_kind_on_every_key() {
        for keys in [1usize, 2, 3] {
            let pairs: BTreeSet<(String, String)> = (0..(4 * keys as u64))
                .map(|id| {
                    (
                        lifecycle_key_for(id, keys),
                        format!("{:?}", lifecycle_op_for(id)),
                    )
                })
                .collect();
            for k in 0..keys {
                for op in ["Edit", "Delete", "Recreate"] {
                    assert!(
                        pairs.contains(&(format!("life-{k}"), op.to_string())),
                        "keys={keys}: life-{k} never gets {op}: {pairs:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn raw_state_classifies_records() {
        let key = RecordKey::new("soak", "ha", "life-0");
        let rec = build_record(&key, b"op-1", None);
        let tomb = tombstone_of(&rec, 1, DEFAULT_ANCESTOR_CAP, None);

        assert_eq!(raw_state(Ok(None)), RawState::Absent);
        assert_eq!(
            raw_state(Ok(Some(rec.clone()))),
            RawState::Live {
                revision: format!("0:{}", rec.revision.hash.0)
            }
        );
        assert_eq!(
            raw_state(Ok(Some(tomb.clone()))),
            RawState::Tombstone {
                revision: format!("1:{}", tomb.revision.hash.0)
            }
        );
        assert!(matches!(
            raw_state(Err(gonzalo_core::CoreError::Backend("down".into()))),
            RawState::ReadFailed { .. }
        ));
    }

    /// End-to-end workload against three in-process `FsStore` replicas over one
    /// shared directory — real concurrency + real conditional-write arbitration,
    /// no external S3 backend. Proves the RMW/lifecycle/oracle/dispatch
    /// integration holds every invariant, including replicated deletion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_process_fs_replicas_hold_the_invariant() {
        let dir = tempfile::tempdir().unwrap();
        let replicas: Vec<Arc<dyn Store>> = (0..3)
            .map(|_| Arc::new(FsStore::new(dir.path())) as Arc<dyn Store>)
            .collect();
        let dispatcher = Arc::new(Dispatcher::new(replicas));

        let cfg = WorkloadConfig {
            writers: 6,
            shared_keys: 3,
            ops_per_writer: 30,
            unique_per_writer: 3,
            lifecycle_keys: 2,
            lifecycle_ops_per_writer: 40,
            unique_deletes_per_writer: 1,
            ..Default::default()
        };
        let mut stats = run(dispatcher.clone(), cfg.clone()).await;
        stats.lifecycle = collect_lifecycle(dispatcher.replicas(), &cfg).await;

        let violations = crate::oracle::check(&stats);
        assert!(violations.is_empty(), "invariant violated: {violations:?}");

        // Belt-and-braces on top of the oracle: the new streams really ran.
        assert!(
            stats
                .lifecycle_ops
                .iter()
                .any(|o| o.op == LifecycleOp::Recreate && o.result == LifecycleResult::Committed)
        );
        assert_eq!(
            stats.unique.iter().filter(|u| u.deleted).count(),
            cfg.writers,
            "one acked delete per writer"
        );
        assert_eq!(stats.lifecycle.len(), cfg.lifecycle_keys);
        assert!(stats.lifecycle.iter().all(|fl| fl.replicas.len() == 3));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-soak --lib workload`
Expected: compile FAIL with `cannot find function lifecycle_key_for` and `struct WorkloadConfig has no field named lifecycle_keys`.

- [ ] **Step 3: Implement**

Replace everything in `crates/gonzalo-soak/src/workload.rs` **above** `#[cfg(test)]` with:

```rust
//! The concurrent write workload and result collection for the [`oracle`].
//!
//! Each writer interleaves three op streams against the replica [`Dispatcher`]:
//!
//! - **Contended RMW** on a small set of shared keys — read the current record,
//!   append this op's globally-unique id to the record's comma-separated set,
//!   and conditionally `put` against the revision just read. On `Conflict`,
//!   re-read and retry (bounded). This is the arbitration proof: if conditional
//!   writes are correct, every *committed* op-id survives in the final set.
//! - **Lifecycle ops** (#203) on a small set of `life-*` keys — edit, delete, or
//!   recreate, chosen by a per-run lifecycle counter ([`lifecycle_op_for`]). Each
//!   op reads the key, then issues one conditional write against what it read. A
//!   lost race is recorded as `Conflict` and not retried. Deletes therefore race
//!   edits and recreations across replicas, and afterwards every replica must
//!   agree on whether each key is deleted.
//! - **Unique-key writes** — disjoint keys written once, later read back to prove
//!   no acked write is lost across replica kills. The first
//!   `unique_deletes_per_writer` of each writer's acked keys are then deleted and
//!   must still read as tombstones at the end (no resurrection under churn).
//!
//! After all writers finish, [`run`] reads the contended and unique keys through
//! the dispatcher. The per-replica lifecycle read, [`collect_lifecycle`], is a
//! separate call because it must reach **every** replica directly. The caller runs
//! it once all replicas are live again.
//!
//! [`oracle`]: crate::oracle

use crate::dispatch::Dispatcher;
use crate::oracle::{
    FinalContended, FinalLifecycle, FinalUnique, LifecycleOp, LifecycleRecord, LifecycleResult,
    OpRecord, OpResult, RawState, ReplicaView, SoakStats,
};
use gonzalo_core::{
    Body, CoreError, DeleteResult, Identity, Meta, PutResult, Record, RecordKey, RecordKind,
    Revision, Store,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Workload shape. The bounded gate and the deep soak both use this; they differ
/// only in the magnitudes and (for the deep soak) running against a duration.
#[derive(Debug, Clone)]
pub struct WorkloadConfig {
    pub namespace: String,
    pub collection: String,
    pub writers: usize,
    pub shared_keys: usize,
    pub ops_per_writer: usize,
    pub unique_per_writer: usize,
    pub max_conflict_retries: usize,
    /// Number of `life-*` keys the lifecycle stream races on. Keep it small so
    /// deletes genuinely contend.
    pub lifecycle_keys: usize,
    /// Lifecycle ops (edit/delete/recreate) per writer. `0` disables the stream
    /// and the oracle's deletion checks.
    pub lifecycle_ops_per_writer: usize,
    /// How many of each writer's acked unique keys it deletes afterwards.
    pub unique_deletes_per_writer: usize,
}

impl Default for WorkloadConfig {
    fn default() -> Self {
        Self {
            namespace: "soak".into(),
            collection: "ha".into(),
            writers: 8,
            shared_keys: 4,
            ops_per_writer: 25,
            unique_per_writer: 4,
            max_conflict_retries: 50,
            lifecycle_keys: 2,
            lifecycle_ops_per_writer: 20,
            unique_deletes_per_writer: 1,
        }
    }
}

/// The op a lifecycle id performs: a quarter deletes, a quarter recreations, and
/// half edits (so deletes race live writes, which is what makes them conflict).
pub fn lifecycle_op_for(life_id: u64) -> LifecycleOp {
    match life_id % 4 {
        0 => LifecycleOp::Delete,
        1 => LifecycleOp::Recreate,
        _ => LifecycleOp::Edit,
    }
}

/// The key a lifecycle id targets. Divides by 4 (the op cycle length) before
/// taking the modulus so the key and the op are decorrelated: every key sees
/// every op kind.
pub fn lifecycle_key_for(life_id: u64, lifecycle_keys: usize) -> String {
    format!(
        "life-{}",
        ((life_id / 4) as usize) % lifecycle_keys.max(1)
    )
}

/// Classify one raw read for the oracle. Revisions render as `"<counter>:<hash>"`.
pub fn raw_state(read: gonzalo_core::Result<Option<Record>>) -> RawState {
    match read {
        Ok(None) => RawState::Absent,
        Ok(Some(rec)) if rec.is_tombstone() => RawState::Tombstone {
            revision: rev_string(&rec.revision),
        },
        Ok(Some(rec)) => RawState::Live {
            revision: rev_string(&rec.revision),
        },
        Err(e) => RawState::ReadFailed {
            error: e.to_string(),
        },
    }
}

fn rev_string(r: &Revision) -> String {
    format!("{}:{}", r.counter, r.hash.0)
}

/// Run the workload to completion and collect stats for the oracle. `dispatcher`
/// fans ops across the live replicas; chaos (replica kills) is driven separately
/// by the caller while this runs. `lifecycle` is left empty: fill it with
/// [`collect_lifecycle`] once every replica is live.
pub async fn run(dispatcher: Arc<Dispatcher>, cfg: WorkloadConfig) -> SoakStats {
    let op_ids = Arc::new(AtomicU64::new(1));
    let life_ids = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for w in 0..cfg.writers {
        let d = dispatcher.clone();
        let c = cfg.clone();
        let ids = op_ids.clone();
        let lids = life_ids.clone();
        handles.push(tokio::spawn(async move { writer(w, d, c, ids, lids).await }));
    }

    let mut ops = Vec::new();
    let mut lifecycle_ops = Vec::new();
    let mut unique_acked: Vec<(String, Vec<u8>)> = Vec::new();
    let mut unique_deleted: BTreeSet<String> = BTreeSet::new();
    let mut conflicts_observed = 0u64;
    let mut writers_completed = 0;
    for h in handles {
        if let Ok(res) = h.await {
            ops.extend(res.ops);
            lifecycle_ops.extend(res.lifecycle);
            unique_acked.extend(res.unique_acked);
            unique_deleted.extend(res.unique_deleted);
            conflicts_observed += res.conflicts;
            writers_completed += 1;
        }
    }

    let contended = collect_contended(&dispatcher, &cfg).await;
    let unique = collect_unique(&dispatcher, &cfg, &unique_acked, &unique_deleted).await;

    SoakStats {
        ops,
        contended,
        unique,
        conflicts_observed,
        writers_completed,
        writers_total: cfg.writers,
        lifecycle_ops,
        lifecycle: Vec::new(),
    }
}

/// Read every lifecycle key from **every** replica directly (raw and consumer).
/// Call only after the writers have stopped and all replicas are live: a dead
/// replica's reads show up as `FinalReadFailed` violations.
pub async fn collect_lifecycle(
    replicas: &[Arc<dyn Store>],
    cfg: &WorkloadConfig,
) -> Vec<FinalLifecycle> {
    let mut out = Vec::with_capacity(cfg.lifecycle_keys);
    for i in 0..cfg.lifecycle_keys {
        let key_id = format!("life-{i}");
        let key = RecordKey::new(&cfg.namespace, &cfg.collection, &key_id);
        let mut views = Vec::with_capacity(replicas.len());
        for store in replicas {
            let raw = raw_state(store.get_raw(&key).await);
            let consumer_live = store.get(&key).await.ok().map(|r| r.is_some());
            views.push(ReplicaView { raw, consumer_live });
        }
        out.push(FinalLifecycle {
            key: key_id,
            replicas: views,
        });
    }
    out
}

struct WriterResult {
    ops: Vec<OpRecord>,
    lifecycle: Vec<LifecycleRecord>,
    unique_acked: Vec<(String, Vec<u8>)>,
    unique_deleted: Vec<String>,
    conflicts: u64,
}

async fn writer(
    w: usize,
    d: Arc<Dispatcher>,
    cfg: WorkloadConfig,
    op_ids: Arc<AtomicU64>,
    life_ids: Arc<AtomicU64>,
) -> WriterResult {
    let mut ops = Vec::with_capacity(cfg.ops_per_writer);
    let mut lifecycle = Vec::with_capacity(cfg.lifecycle_ops_per_writer);
    let mut conflicts = 0u64;
    let rounds = cfg.ops_per_writer.max(cfg.lifecycle_ops_per_writer);
    for i in 0..rounds {
        if i < cfg.ops_per_writer {
            let op_id = op_ids.fetch_add(1, Ordering::Relaxed);
            let key_id = format!("shared-{}", (op_id as usize) % cfg.shared_keys.max(1));
            let (result, seen) = rmw_append(&d, &cfg, &key_id, op_id).await;
            conflicts += seen;
            ops.push(OpRecord {
                key: key_id,
                op_id,
                result,
            });
        }
        if i < cfg.lifecycle_ops_per_writer && cfg.lifecycle_keys > 0 {
            let life_id = life_ids.fetch_add(1, Ordering::Relaxed);
            let key_id = lifecycle_key_for(life_id, cfg.lifecycle_keys);
            let op = lifecycle_op_for(life_id);
            let result = lifecycle_step(&d, &cfg, &key_id, life_id, op).await;
            lifecycle.push(LifecycleRecord {
                key: key_id,
                op_id: life_id,
                op,
                result,
            });
        }
    }

    let mut unique_acked = Vec::with_capacity(cfg.unique_per_writer);
    for i in 0..cfg.unique_per_writer {
        let key_id = format!("unique-{w}-{i}");
        let value = key_id.clone().into_bytes();
        if create(&d, &cfg, &key_id, &value).await {
            unique_acked.push((key_id, value));
        }
    }

    let mut unique_deleted = Vec::new();
    for (key_id, _) in unique_acked.iter().take(cfg.unique_deletes_per_writer) {
        if delete_unique(&d, &cfg, key_id).await {
            unique_deleted.push(key_id.clone());
        }
    }

    WriterResult {
        ops,
        lifecycle,
        unique_acked,
        unique_deleted,
        conflicts,
    }
}

/// Read-modify-write: append `op_id` to a shared key's set under a conditional
/// put, retrying on `Conflict`. Returns the op's final outcome and the number of
/// transient conflicts observed while racing to commit it.
async fn rmw_append(
    d: &Dispatcher,
    cfg: &WorkloadConfig,
    key_id: &str,
    op_id: u64,
) -> (OpResult, u64) {
    let key = RecordKey::new(&cfg.namespace, &cfg.collection, key_id);
    let mut conflicts = 0u64;
    for _ in 0..=cfg.max_conflict_retries {
        let current = match d.get(&key).await {
            Ok(c) => c,
            Err(_) => return (OpResult::Failed, conflicts),
        };
        let (mut members, expected) = match &current {
            Some(rec) => (parse_members(rec.body.bytes()), Some(rec.revision.clone())),
            None => (Vec::new(), None),
        };
        if !members.contains(&op_id) {
            members.push(op_id);
        }
        let record = build_record(&key, &encode_members(&members), expected.clone());
        match d.put(record, expected).await {
            Ok(PutResult::Committed(_)) => return (OpResult::Committed, conflicts),
            Ok(PutResult::Conflict(_)) => {
                conflicts += 1;
                continue;
            }
            Err(_) => return (OpResult::Failed, conflicts),
        }
    }
    // Exhausted the retry budget without winning the race — did not commit.
    (OpResult::Conflict, conflicts)
}

/// One lifecycle op: read, then a single conditional write against what was read.
async fn lifecycle_step(
    d: &Dispatcher,
    cfg: &WorkloadConfig,
    key_id: &str,
    life_id: u64,
    op: LifecycleOp,
) -> LifecycleResult {
    let key = RecordKey::new(&cfg.namespace, &cfg.collection, key_id);
    let current = match d.get(&key).await {
        Ok(c) => c,
        Err(_) => return LifecycleResult::Failed,
    };
    // Let other writers run between the read and the write, so deletes really do
    // race edits and recreations instead of completing uncontended.
    tokio::task::yield_now().await;
    let body = format!("op-{life_id}");
    match (op, current) {
        (LifecycleOp::Delete, Some(rec)) => match d.delete(&key, Some(rec.revision)).await {
            Ok(DeleteResult::Deleted) => LifecycleResult::Committed,
            Ok(DeleteResult::Conflict(_)) => LifecycleResult::Conflict,
            Err(_) => LifecycleResult::Failed,
        },
        (LifecycleOp::Edit, Some(rec)) => {
            let record = build_record(&key, body.as_bytes(), Some(rec.revision.clone()));
            lifecycle_put_result(d.put(record, Some(rec.revision)).await)
        }
        (LifecycleOp::Recreate, None) => {
            // Over a tombstone the store re-stamps this counter-0 revision to
            // continue the chain (spec §3.2), so no special handling here.
            let record = build_record(&key, body.as_bytes(), None);
            lifecycle_put_result(d.put(record, None).await)
        }
        (LifecycleOp::Delete, None) | (LifecycleOp::Edit, None) | (LifecycleOp::Recreate, Some(_)) => {
            LifecycleResult::Skipped
        }
    }
}

fn lifecycle_put_result(r: gonzalo_core::Result<PutResult>) -> LifecycleResult {
    match r {
        Ok(PutResult::Committed(_)) => LifecycleResult::Committed,
        Ok(PutResult::Conflict(_)) => LifecycleResult::Conflict,
        // A consumer put with `Some(expected)` over a tombstone is always
        // `NotFound` (the key reads as absent): an edit that lost a race to a
        // delete. That's a lost race, not a failure.
        Err(CoreError::NotFound(_)) => LifecycleResult::Conflict,
        Err(_) => LifecycleResult::Failed,
    }
}

/// Create a unique key once. Returns `true` if the write was acked (`Committed`).
async fn create(d: &Dispatcher, cfg: &WorkloadConfig, key_id: &str, value: &[u8]) -> bool {
    let key = RecordKey::new(&cfg.namespace, &cfg.collection, key_id);
    let record = build_record(&key, value, None);
    matches!(d.put(record, None).await, Ok(PutResult::Committed(_)))
}

/// Delete an (uncontended) unique key against its current revision. Returns
/// `true` if the delete was acked.
async fn delete_unique(d: &Dispatcher, cfg: &WorkloadConfig, key_id: &str) -> bool {
    let key = RecordKey::new(&cfg.namespace, &cfg.collection, key_id);
    let Ok(Some(rec)) = d.get(&key).await else {
        return false;
    };
    matches!(
        d.delete(&key, Some(rec.revision)).await,
        Ok(DeleteResult::Deleted)
    )
}

async fn collect_contended(d: &Dispatcher, cfg: &WorkloadConfig) -> Vec<FinalContended> {
    let mut out = Vec::new();
    for i in 0..cfg.shared_keys {
        let key_id = format!("shared-{i}");
        let key = RecordKey::new(&cfg.namespace, &cfg.collection, &key_id);
        let members = match d.get(&key).await {
            Ok(Some(rec)) => parse_members(rec.body.bytes()),
            _ => Vec::new(),
        };
        out.push(FinalContended {
            key: key_id,
            members,
        });
    }
    out
}

async fn collect_unique(
    d: &Dispatcher,
    cfg: &WorkloadConfig,
    acked: &[(String, Vec<u8>)],
    deleted: &BTreeSet<String>,
) -> Vec<FinalUnique> {
    let mut out = Vec::new();
    for (key_id, value) in acked {
        let key = RecordKey::new(&cfg.namespace, &cfg.collection, key_id);
        let readable_with_value = matches!(
            d.get(&key).await,
            Ok(Some(rec)) if rec.body.bytes() == value.as_slice()
        );
        let was_deleted = deleted.contains(key_id);
        let raw_tombstone = was_deleted
            && matches!(d.get_raw(&key).await, Ok(Some(rec)) if rec.is_tombstone());
        out.push(FinalUnique {
            key: key_id.clone(),
            acked: true,
            readable_with_value,
            deleted: was_deleted,
            raw_tombstone,
        });
    }
    out
}

fn build_record(key: &RecordKey, body_bytes: &[u8], parent: Option<Revision>) -> Record {
    let body = Body::Inline(body_bytes.to_vec());
    let revision = match &parent {
        Some(p) => p.next(body.bytes()),
        None => Revision::initial(body.bytes()),
    };
    Record {
        revision,
        parent,
        body,
        kind: RecordKind::Topic,
        meta: Meta {
            author: Identity::new("soak"),
            origin_system: "soak".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        key: key.clone(),
        ancestors: Vec::new(),
        deleted_at: None,
    }
}

fn parse_members(bytes: &[u8]) -> Vec<u64> {
    std::str::from_utf8(bytes)
        .unwrap_or("")
        .split(',')
        .filter_map(|s| s.trim().parse::<u64>().ok())
        .collect()
}

fn encode_members(members: &[u64]) -> Vec<u8> {
    members
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",")
        .into_bytes()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-soak --lib workload`
Expected: PASS, `test result: ok. 4 passed; 0 failed`.

Run it five more times to check for flakiness in the delete-conflict check. It relies on real contention:

```bash
for i in 1 2 3 4 5; do cargo test -q -p gonzalo-soak --lib workload::tests::in_process_fs_replicas_hold_the_invariant 2>&1 | tail -1; done
```

Expected: five lines of `test result: ok. 1 passed; 0 failed`. If any run reports `NoDeleteConflictsObserved`, raise `lifecycle_ops_per_writer` in that test from `40` to `80`, re-run the loop, and note it in the commit message. Don't add sleeps.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-soak/src/workload.rs
git commit -m "feat(soak): delete/recreate lifecycle stream and per-replica collection

Writers interleave edit/delete/recreate ops on life-* keys and delete
some acked unique keys; collect_lifecycle reads every replica's raw and
consumer view for the oracle (#203).

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 4: Harness, binary, bounded gate, and CI trigger

**Files:**
- Modify: `crates/gonzalo-soak/src/harness.rs:57-81`
- Modify: `crates/gonzalo-soak/src/main.rs`
- Modify: `crates/gonzalo-soak/tests/ha_soak.rs`
- Modify: `.github/workflows/ha-soak.yml:14-21`

**Interfaces:**
- Consumes: `workload::collect_lifecycle`, `Dispatcher::replicas`, `oracle::delete_conflicts`, the new `WorkloadConfig` fields.
- Produces: `gonzalo-soak` flags `--lifecycle-keys N`, `--lifecycle-ops-per-writer N`, `--unique-deletes-per-writer N`.

- [ ] **Step 1: Collect per-replica state in the harness**

In `crates/gonzalo-soak/src/harness.rs`, replace the body of the `for r in 0..rounds { ... }` loop with:

```rust
    for r in 0..rounds {
        let mut cfg = base_cfg.clone();
        cfg.collection = format!("{}-r{r}", base_cfg.collection);
        let round_cfg = cfg.clone();

        let workload_task = {
            let d = dispatcher.clone();
            tokio::spawn(async move { workload::run(d, cfg).await })
        };

        if replicas > 1 {
            // Warm up, kill a rotating replica mid-load (a pod death), hold,
            // then recover it — the failover path a k8s Service masks.
            let victim = 1 + (r % (replicas - 1));
            tokio::time::sleep(Duration::from_millis(200)).await;
            set.kill(victim);
            tokio::time::sleep(Duration::from_millis(450)).await;
            set.respawn(victim).await?;
        }

        let mut stats = workload_task
            .await
            .map_err(|e| format!("workload task panicked: {e}"))?;
        // Settled: the writers have stopped and the victim answered /readyz again,
        // so every replica is live. The replicas front one shared bucket, so there
        // is no replication lag to wait out. Read every replica directly.
        stats.lifecycle = workload::collect_lifecycle(dispatcher.replicas(), &round_cfg).await;
        let violations = oracle::check(&stats);
        outcomes.push(SoakOutcome { stats, violations });
    }
```

Also add this paragraph to the module doc comment, after its first paragraph:

```rust
//!
//! After each round the harness reads every lifecycle key from **every** replica
//! (`workload::collect_lifecycle`), so the oracle can check that the replicas agree
//! on which keys are deleted (#203).
```

- [ ] **Step 2: Add flags and delete-conflict reporting to the binary**

In `crates/gonzalo-soak/src/main.rs`:

Replace the `//! Usage:` block in the module doc with:

```rust
//! Usage:
//!   gonzalo-soak [--replicas N] [--rounds N] [--writers N] [--shared-keys N]
//!                [--ops-per-writer N] [--unique-per-writer N] [--retries N]
//!                [--lifecycle-keys N] [--lifecycle-ops-per-writer N]
//!                [--unique-deletes-per-writer N]
```

Replace the `let mut cfg = WorkloadConfig { ... };` in `parse_args` with:

```rust
    let mut cfg = WorkloadConfig {
        writers: 16,
        shared_keys: 6,
        ops_per_writer: 50,
        unique_per_writer: 4,
        max_conflict_retries: 100,
        lifecycle_keys: 3,
        lifecycle_ops_per_writer: 50,
        unique_deletes_per_writer: 2,
        ..Default::default()
    };
```

Add three arms to the `match flag.as_str()`, directly after the `"--retries"` arm:

```rust
            "--lifecycle-keys" => cfg.lifecycle_keys = val()?,
            "--lifecycle-ops-per-writer" => cfg.lifecycle_ops_per_writer = val()?,
            "--unique-deletes-per-writer" => cfg.unique_deletes_per_writer = val()?,
```

Replace the startup `eprintln!` with:

```rust
    eprintln!(
        "gonzalo-soak: {} replicas, {} rounds, {} writers, {} shared keys, {} lifecycle keys",
        args.replicas, args.rounds, args.cfg.writers, args.cfg.shared_keys, args.cfg.lifecycle_keys
    );
```

Replace the PASS `eprintln!` inside `if o.passed()` with:

```rust
            eprintln!(
                "round {r}: PASS  committed={committed} conflicts={} delete_conflicts={} writers={}/{}",
                o.stats.conflicts_observed,
                gonzalo_soak::oracle::delete_conflicts(&o.stats),
                o.stats.writers_completed,
                o.stats.writers_total
            );
```

Replace `fn usage()` with:

```rust
fn usage() -> &'static str {
    "usage: gonzalo-soak [--replicas N] [--rounds N] [--writers N] \
     [--shared-keys N] [--ops-per-writer N] [--unique-per-writer N] [--retries N] \
     [--lifecycle-keys N] [--lifecycle-ops-per-writer N] [--unique-deletes-per-writer N]"
}
```

- [ ] **Step 3: Configure the bounded gate**

In `crates/gonzalo-soak/tests/ha_soak.rs`, replace the module doc's second paragraph (the one starting `Spawns 3 real`) with:

```rust
//! Spawns 3 real `gonzalod` replicas over a shared S3 backend, runs a
//! contended + lifecycle (edit/delete/recreate) + unique-key workload while
//! killing and recovering one replica, and asserts gonzalo's invariants (no lost
//! update, conflicts surface for writes and deletes, durability of writes and
//! deletes, replicas agree on deletion, liveness). **Skips** (does not fail) unless a S3 target is configured via
```

(Keep the remaining doc lines from `//! \`GONZALO_S3_TEST_ENDPOINT\`` onward unchanged.)

Replace the `let cfg = WorkloadConfig { ... };` with:

```rust
    let cfg = WorkloadConfig {
        writers: 8,
        shared_keys: 4,
        ops_per_writer: 25,
        unique_per_writer: 3,
        max_conflict_retries: 50,
        lifecycle_keys: 2,
        lifecycle_ops_per_writer: 25,
        unique_deletes_per_writer: 1,
        ..Default::default()
    };
```

Replace the final `assert!(...)` with:

```rust
    assert!(
        outcome.passed(),
        "HA soak invariant violations: {:?}\n\
         committed={committed} conflicts={} delete_conflicts={} writers={}/{}",
        outcome.violations,
        outcome.stats.conflicts_observed,
        gonzalo_soak::oracle::delete_conflicts(&outcome.stats),
        outcome.stats.writers_completed,
        outcome.stats.writers_total,
    );
```

- [ ] **Step 4: Trigger the soak when core or the client changes**

The soak now exercises core's tombstone planners and `ServerStore`'s raw routes. In `.github/workflows/ha-soak.yml`, replace the `paths:` list under `pull_request` with:

```yaml
    paths:
      - "crates/gonzalo-soak/**"
      - "crates/gonzalo-core/**"
      - "crates/gonzalo-store-s3/**"
      - "crates/gonzalo-store-server/**"
      - "crates/gonzalo-server/**"
      - "docker-compose.rustfs.yml"
      - "scripts/rustfs-*.sh"
      - ".github/workflows/ha-soak.yml"
```

- [ ] **Step 5: Verify without S3**

Run: `cargo build -p gonzalo-soak --all-targets`
Expected: `Finished` with no warnings.

Run: `cargo test -p gonzalo-soak`
Expected: PASS. The output includes `skipping ha_soak_bounded: set GONZALO_S3_TEST_ENDPOINT, ...` (only with `-- --nocapture`), `ha_soak_bounded ... ok`, and all lib tests ok.

Run: `cargo run -q -p gonzalo-soak -- --help`
Expected: exit 0, and stderr prints a usage line containing `[--lifecycle-keys N] [--lifecycle-ops-per-writer N] [--unique-deletes-per-writer N]`.

- [ ] **Step 6: Verify against RustFS (requires docker)**

```bash
cargo build -p gonzalo-server --bin gonzalod
cargo build -p gonzalo-soak --tests
eval "$(bash scripts/rustfs-up.sh)"
cargo test -p gonzalo-soak --test ha_soak -- --nocapture
bash scripts/rustfs-down.sh --purge
```

Expected: `test ha_soak_bounded ... ok` and `test result: ok. 1 passed`. If docker is unavailable, skip this step and write "RustFS gate not run locally; relying on the ha-soak workflow" in the PR body. The workflow runs on this PR because `crates/gonzalo-soak/**` changed.

Optional deep check with RustFS still up: `cargo run -p gonzalo-soak -- --rounds 3 --replicas 3`. Expected: three `round N: PASS ... delete_conflicts=<non-zero>` lines, then `gonzalo-soak: all 3 rounds passed`.

- [ ] **Step 7: Commit**

```bash
git add crates/gonzalo-soak/src/harness.rs crates/gonzalo-soak/src/main.rs crates/gonzalo-soak/tests/ha_soak.rs .github/workflows/ha-soak.yml
git commit -m "feat(soak): check replicated deletion in the bounded gate and deep soak

The harness reads every replica after respawn; the binary gains lifecycle
flags and reports delete conflicts; ha-soak also triggers on core and
store-server changes (#203).

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 5: ADR 0021 and the ADR 0018 back-reference

**Files:**
- Create: `docs/adr/0021-replicated-deletion-with-tombstones.md`
- Modify: `docs/adr/README.md:45` (0018 row) and add a row after `:47` (0020 row)

**Interfaces:**
- Consumes: slice 1–6 file layout. Every path the ADR cites must exist on `main`: `crates/gonzalo-core/src/tombstone.rs`, `crates/gonzalo-core/src/sync.rs`, `crates/gonzalo-core/src/conformance.rs`, `crates/gonzalo-soak/`.
- Produces: ADR 0021 (cited by the guide page and CHANGELOG in Tasks 6–7).

- [ ] **Step 1: Confirm the numbering and cited paths**

```bash
ls docs/adr | rg '^\d{4}-' | tail -3
ls crates/gonzalo-core/src/tombstone.rs crates/gonzalo-core/src/sync.rs crates/gonzalo-core/src/conformance.rs
ls -d crates/gonzalo-soak
```

Expected: the last ADR is `0020-rust-native-deliverables.md` and all four paths list. If another ADR has taken 0021 in the meantime, stop and renumber throughout this plan (filename, heading, README row, and the links in Tasks 6–7).

- [ ] **Step 2: Write the ADR**

Create `docs/adr/0021-replicated-deletion-with-tombstones.md` with exactly:

````markdown
# ADR 0021 · Replicated deletion with tombstones

- **Status:** accepted
- **Date:** 2026-09-13
- **Source:** [`docs/superpowers/specs/2026-09-13-tombstone-replication-design.md`](../superpowers/specs/2026-09-13-tombstone-replication-design.md)

## Context

**This ADR supersedes the local-only deletion decision of
[ADR 0018](0018-record-deletion-and-sync.md).** ADR 0018's OCC semantics for
`delete` still hold: an `expected` revision, `Conflict` on a mismatch, and
atomicity in the same critical section as `put`. Only its choice to make deletion
local-only, which it deferred as a known sharp edge, is replaced here.

Under ADR 0018, `delete` physically removed a record. Gonzalo replicates in two
ways: `sync` takes the union of two stores' keys and copies one-sided records
across, and git `pull` (ADR 0017) runs a three-way merge. A physical delete
leaves no trace, so when sync met a peer that still held the record, "deleted
here" and "never existed here" looked the same and sync copied the record back.
**A deleted record came back on the next sync.**

gonzalo#203 asked for a first-class namespace reset: clear a namespace or a
collection in one call, with the same meaning on every substrate. A reset built
on local-only delete has the same resurrection problem at scale: reset a namespace
on a laptop, sync with the team daemon, and every record returns. Gonzalo targets
deployments from one local directory up to replicated daemons over shared object
storage, and a reset that only works if you never sync doesn't meet that. So
deletion had to replicate before reset could be built.

Options weighed:

- **Local-only reset.** Simple, and it resurrects on the first sync. Unusable as
  soon as a namespace is replicated. Rejected.
- **Hybrid: ship local-only reset now, tombstones later.** This ships a reset
  whose meaning changes in a later release, which is worse than waiting for the
  right meaning. Rejected.
- **Tombstones.** Deletion becomes a replicated write. Taken, with the details
  below.

## Decision

We will make deletion a **replicated write**.

**Tombstone record.** `delete` writes a record of the new kind
`RecordKind::Tombstone` at the key's normal path. There is no side index and no
new storage layout. The tombstone has an empty inline body, `parent` set to the
deleted revision, and `deleted_at` set to the deletion time in milliseconds since
the Unix epoch. Its revision is `{ counter: deleted.counter + 1, hash:
ContentHash::of(b"gonzalo:tombstone:v1") }`. The domain-separated hash means a
tombstone's revision can never equal a live edit's revision. If tombstones used
the empty-body hash, a live edit to an empty body at the same counter would look
"already in sync" with a concurrent delete. The same construction makes two peers
that independently delete the same revision produce identical tombstones, which
sync treats as already in sync. The shared helpers live in
`crates/gonzalo-core/src/tombstone.rs`.

**Two surfaces.** Consumer reads (`get`, `list`) hide tombstones, so a deleted
key looks absent to applications. The replication surface shows them, and `sync`,
`pull` and collection use only that surface:

- `get_raw` and `list_raw` read tombstones;
- `put_raw(record, expected)` writes a record exactly as given;
- `purge` physically removes a record if its revision matches, and is now the only
  physical removal in the system.

All four are **required** `Store` methods with no default implementation. A default
of `get_raw = get` or `put_raw = put` would compile, pass every test that doesn't
involve deletion, and resurrect records in production. For the same reason, the
daemon client never falls back from a replication call to a consumer call: against
a daemon that predates these routes it returns an explicit upgrade error.

**Consumer writes.** `delete` of a live record writes the tombstone, or conflicts
on a stale `expected`. `delete` of a tombstone or an absent key writes nothing and
returns `Deleted`. An absent key has no revision a tombstone could descend from. A
consumer `put` with `expected = None` over a tombstone is a **recreation**. The
store re-stamps the caller's revision to `tombstone.counter + 1` with the
tombstone as `parent`, so the recreated record is newer than the delete instead of
losing to it. A consumer `put` with any `Some(expected)` over a tombstone returns
`NotFound`, because to a consumer the key is absent.

**Replication writes never re-stamp.** `put_raw` follows one rule:

- nothing stored and no `expected`: write;
- nothing stored but `Some(expected)`: `NotFound`;
- `expected` equals the stored revision, tombstone or not: write the record
  verbatim;
- anything else: `Conflict`, carrying the stored record, which may be a tombstone.

This closes a resurrection window. Sync copies a record that exists on only one
side after a raw read of the other side. If that copy went through consumer
`put(record, None)`, a delete landing on the destination between the raw read and
the write would turn the copy into a recreation, re-stamped newer than the delete,
and the deleted record would come back. Through `put_raw` the same race is a
`Conflict` that the next sync pass resolves with the tombstone in view.

**Authorship.** `delete_as(key, expected, author: Option<Identity>)` is the
required delete method, and `delete` is a provided method that passes no author. A
tombstone carries the deleted record's metadata. When an author is given, it
becomes the tombstone's author, so the record of a delete names who deleted it
rather than who last edited it. The daemon passes the authenticated principal, as
it already does for `put`, and `gonzalo delete` passes `gonzalo-cli`. `reset` goes
through the provided `delete`, so its tombstones carry no author of their own.

Replication writes follow a separate author rule. `put_raw` never re-stamps a
record's *revision*, but the daemon does apply ADR 0015's unforgeable authorship to
its author. A `put_raw` from a non-admin principal has `meta.author` restamped to
that principal, exactly like a consumer `put`. A `put_raw` from an admin principal,
or on a daemon running without auth, keeps the replicated record's author. An admin
token is the replication credential, and only it can carry another writer's
authorship across stores. Without this rule, any principal with `write` on a
namespace could forge records attributed to someone else by sending them through the
raw route.

A consumer `put` or `put_raw` that the store rejects with `NotFound`, such as a
`Some(expected)` over a tombstone, is returned by the daemon as HTTP 412 or gRPC
`FailedPrecondition`. The client maps that back to `NotFound`. 404 is not used,
because on the raw routes a 404 is how the client recognises a daemon that predates
them.

Every substrate makes these decisions through the same pure planner functions in
core, so they behave identically by construction. The conformance suite
(`crates/gonzalo-core/src/conformance.rs`) proves it on each substrate. The lock-based
stores (fs, git) plan inside their lock. s3 has no lock: its writes are
conditional on the ETag it read. When a conditional write loses a race —
`PreconditionFailed` (HTTP 412), `ConditionalRequestConflict` (HTTP 409), or
`NoSuchKey` (the object vanished to a concurrent purge) — s3 re-reads, re-plans
and retries, up to 8 attempts, then returns a backend error. It therefore
reaches the same outcome a lock-based store would, instead of guessing from a
single re-read.

**Ordering by bounded ancestry.** Every record carries `ancestors`, its most recent
prior revisions sorted newest first and capped per store. The cap defaults to 32,
is set with `--ancestor-cap` on each CLI command that opens a store and with the
`GONZALO_ANCESTOR_CAP` environment variable on `gonzalod`, and is never 0. When both sides of a sync hold a key with different revisions, sync
(`crates/gonzalo-core/src/sync.rs`) decides:

- one side's revision is in the other's `ancestors`: that side is behind, and is
  overwritten (a fast-forward);
- neither contains the other, and both are tombstones: the higher
  `(counter, hash)` wins on both sides;
- neither contains the other, and exactly one is a tombstone: a `SyncConflict`,
  with neither side written. A delete racing an edit of the same revision can't be
  resolved without discarding someone's intent, and gonzalo already surfaces
  unmergeable divergence rather than guessing;
- neither contains the other, and both are live: the existing merge path.

A chain longer than the cap looks like divergence, so the bounded list **fails
safe**: a conflict or a merge, never a silent overwrite. Records written before
this change have no ancestors and take the old merge path unchanged. git `pull`
applies the same kind rules to paths changed on both sides of its real merge base.

**Reset.** `reset` tombstones every live record under a prefix that must name a
namespace, using ordinary conditional deletes. It is not atomic, because no
substrate offers multi-key transactions. It is idempotent: a re-run tombstones what
the first run missed and reports keys edited concurrently as conflicts. It needs
only `write` on the namespace.

**CLI.** The CLI works on a local fs store only (`--root <DIR>`, default `.`).
There are three commands:

- `gonzalo delete --namespace <N> --collection <C> --id <I> [--expected <REVISION_JSON>]`, where `--expected` is revision JSON as `gonzalo get` prints it;
- `gonzalo reset --namespace <N> [--collection <C>]`;
- `gonzalo collect --older-than <DURATION> [--namespace <N> [--collection <C>]]`, where `<DURATION>` is a positive integer plus exactly one unit of `d`, `h`, `m` or `s`.

Each also takes `--ancestor-cap <K>`, which defaults to 32; a cap of 0 is an error.

Exit codes are `0` on success, `1` on an error, `2` on a usage error, and **`3` on a
conflict**: a stale `--expected` on delete, or one or more concurrently edited
records on reset (re-run to finish). A conflict is a normal, recoverable outcome
(ADR 0005), so a script can tell "re-read and retry" apart from "something is
broken" without parsing output. `collect` exits `0` even when it reports
conflicts. A collect conflict means the key was recreated during collection, and
there is nothing to retry.

**Collection.** `collect` purges tombstones whose `deleted_at` is at least an
operator-supplied horizon old, conditionally on the tombstone's revision, so a key
recreated meanwhile survives. There is no default horizon, and the CLI requires
`--older-than`, which rejects 0. Tombstones without `deleted_at` are never collected, and a
future-dated `deleted_at` (clock skew) counts as too young. On the daemon, raw
reads need `read` on the namespace, `put_raw` needs `write` on the namespace, and
`purge` and unscoped raw listing need admin (ADR 0015).

**Version.** Required trait methods (`get_raw`, `list_raw`, `put_raw`, `purge`,
`delete_as`) and a new `RecordKind` variant break
`gonzalo-core`'s API, so this ships as 0.7.0 across the lockstep workspace.

Rejected alternatives for the mechanism:

- **Counter comparison for ordering** (higher counter wins). A peer that edits many
  times offline gets a higher counter than a peer that deleted once, so a stale
  edit would beat a newer delete. Counters count edits; they don't order them.
- **Version vectors.** Exact under every topology, but they need an entry per
  writer that grows with every replica and principal. That doesn't fit a record
  format that is also a human-readable file in git.
- **Automatic or background collection.** Purging a tombstone too early is data
  loss that shows up later, on a different machine, when a long-offline peer syncs
  its live copy back. Only the operator knows how long peers stay offline. An
  automatic collector would have to guess, and a wrong guess fails silently.
  Explicit collection keeps that decision with the operator and prints the horizon
  used. A hub that tracks each peer's last sync could make collection safe
  automatically later. That's an optimisation, not a correctness requirement.
- **A separate tombstone index.** A side index would let `list` skip reading
  records, but it is a second structure that must stay consistent with the records
  on every substrate, and it hides deletes from git history. Keeping the tombstone
  at the record's own path shows a delete as an ordinary modification.

## Consequences

- **Positive:** a delete sticks across `sync` and `pull` on every substrate, so
  namespace reset means the same thing locally and replicated. Sync fast-forwards
  exactly when one side is behind: before this, an `Opaque` kind such as
  `Checkpoint` reported a conflict even though nothing had diverged. Concurrent
  deletes converge without coordination, and delete-versus-edit is surfaced rather
  than silently decided. The HA soak (`crates/gonzalo-soak/`) races deletes against
  edits and recreations across daemon replicas under replica-kill chaos, and
  checks the replicas agree on every key's deletion state.
- **Negative:** tombstones take storage until an operator collects them. Consumer
  `list` has to read each record to filter tombstones, which on s3 is a `GetObject`
  per key and expensive for large namespaces. Deleting a blob-backed record doesn't
  reclaim its blob, because blobs are content-addressed and may be shared, so purge
  can't remove them. **Mixed versions are unsafe:** a pre-0.7 binary that reads a
  store holding a tombstone fails on that key with a serialization error (loud, no
  data loss), and a pre-0.7 binary that runs sync can't see tombstones and copies
  deleted records back (silent). Every binary that reads a store or runs sync must
  be upgraded together. The store can't tell that from a genuine recreation, so it
  can't block it. A collection horizon shorter than a peer's offline window
  resurrects records on that peer's next sync. Recreation re-stamps the caller's
  revision, so a caller that ignores the returned revision and reuses its own gets a
  conflict on its next conditional write. Blob garbage collection, `list`
  performance on s3, and populating `Meta.created`/`Meta.updated` are follow-up
  work.
- **Revisit if:** a deployment needs collection without an operator-chosen horizon
  (the trigger for hub-tracked peer sync times); ancestor chains routinely exceed
  the cap so that fast-forwards degrade into conflicts in practice (the trigger for
  a larger default or real causality metadata); or s3 `list` cost on large
  namespaces becomes a bottleneck (the trigger for a key-level tombstone marker,
  which is a layout change needing its own decision).
````

- [ ] **Step 3: Update the README index**

In `docs/adr/README.md`, replace the 0018 row:

```markdown
| [0018](0018-record-deletion-and-sync.md) | Record deletion and its sync semantics | accepted |
```

with:

```markdown
| [0018](0018-record-deletion-and-sync.md) | Record deletion and its sync semantics | accepted (local-only deletion superseded by [0021](0021-replicated-deletion-with-tombstones.md)) |
```

and after the 0020 row, add:

```markdown
| [0021](0021-replicated-deletion-with-tombstones.md) | Replicated deletion with tombstones | accepted |
```

Do **not** edit `docs/adr/0018-record-deletion-and-sync.md`. ADRs are append-only, and a partially superseded ADR keeps status `accepted` in its body. The back-reference lives in the index row. No ADR in this repo has been partially superseded before, so this row sets the convention.

- [ ] **Step 4: Check cited paths and links resolve**

```bash
rg -o '`((crates|docs)/[^`]+)`' -r '$1' docs/adr/0021-replicated-deletion-with-tombstones.md | sort -u | while read -r p; do if [ -e "$p" ]; then echo "ok      $p"; else echo "MISSING $p"; fi; done
(cd docs/adr && rg -o '\]\(([^)#]+\.md)\)' -r '$1' 0021-replicated-deletion-with-tombstones.md README.md | cut -d: -f2 | sort -u | while read -r p; do if [ -e "$p" ]; then echo "ok      $p"; else echo "MISSING $p"; fi; done)
```

Expected: every line starts with `ok`, and none with `MISSING`.

- [ ] **Step 5: Run the ADR validator**

Invoke the `adr-validate` skill (Skill tool, skill `adr-validate`) on `docs/adr/`.
Expected: `PASS` for all six checks: status parity, supersession, self-sustaining, references, sequence integrity, format. In particular, 0018's `accepted` body must pair with its `accepted (local-only deletion superseded by [0021](…))` index row, and 0021's Context must name 0018. Apply any mechanical fixes it offers only to 0021 or the README rows, never to 0018's body, and re-run until clean.

- [ ] **Step 6: Commit**

```bash
git add docs/adr/0021-replicated-deletion-with-tombstones.md docs/adr/README.md
git commit -m "docs(adr): 0021 replicated deletion with tombstones

Supersedes ADR 0018's local-only deletion decision; 0018 stays accepted
with a back-reference in the index (#203).

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 6: Guide page

**Files:**
- Create: `docs/guide/src/deletion.md`
- Modify: `docs/guide/src/SUMMARY.md:11` (Guides list; the ADR block after `<!-- adrs -->` is regenerated)
- Modify: `docs/evaluation/competitors/mem0/parity-gap-matrix.md:32`

**Interfaces:**
- Consumes: the final CLI synopsis from the revised slice 6 plan (`2026-09-13-tombstones-06-reset-collect-cli.md`). The guide, ADR 0021 and CHANGELOG match it exactly:

  Common to all three commands: `--root <DIR>` (default `.`, expands a leading `~`) and `--ancestor-cap <K>` (default 32; `0` exits 1 with `ancestor cap must be at least 1`). The CLI opens only a local fs store.

| Command | stdout | stderr | Exit |
|---|---|---|---|
| `gonzalo delete --namespace <N> --collection <C> --id <I> [--expected <REVISION_JSON>] [--root <DIR>] [--ancestor-cap <K>]`, deleted, absent or already deleted | `deleted: N/C/I` | — | 0 |
| delete, stale `--expected` | `conflict: N/C/I` then `current:  {"counter":…,"hash":"…"}` | — | 3 |
| delete, malformed `--expected` | — | contains `expected a revision as JSON` | 2 |
| `gonzalo reset --namespace <N> [--collection <C>] [--root <DIR>] [--ancestor-cap <K>]` | `X deleted, M conflicts` | one `conflict: <ns/col/id>` per conflict | 0 if M == 0; 3 if M > 0 |
| reset without `--namespace` | — | clap usage error | 2 |
| `gonzalo collect --older-than <DURATION> [--namespace <N> [--collection <C>]] [--root <DIR>] [--ancestor-cap <K>]` | `horizon:   <as typed> (<seconds>s)`, `purged:    P`, `unstamped: U`, `conflicts: Q` | one `conflict: <key>` per conflict | 0, even with conflicts |
| collect: missing `--older-than`; zero, missing unit, compound or unknown unit, or overflow; `--collection` without `--namespace` | — | clap usage error | 2 |
| any other error (I/O, store, `--ancestor-cap 0`) | — | error message | 1 |

  - `delete` tombstones are attributed to author `gonzalo-cli`. `reset` tombstones carry no author, because reset goes through the provided `delete`.
  - `--older-than` is a positive integer plus exactly one unit of `d`, `h`, `m` or `s`, with no default. Without `--namespace`, collect covers the whole store.
  - `gonzalo sync <A> <B>` gains `[--ancestor-cap <K>]`.
  - `gonzalod` reads its cap only from `GONZALO_ANCESTOR_CAP`. The daemon adds `PUT /v1/raw/records/{ns}/{col}/{id}` / `PutRaw` (`write`) to the raw routes.
- Produces: guide page `deletion.md`.

- [ ] **Step 1: Compare the shipped CLI with the synopsis above**

```bash
cargo run -q -p gonzalo-cli -- delete --help
cargo run -q -p gonzalo-cli -- reset --help
cargo run -q -p gonzalo-cli -- collect --help
cargo run -q -p gonzalo-cli -- sync --help
```

Expected:
- `delete` lists `--namespace`, `--collection`, `--id`, `--expected`, `--root` and `--ancestor-cap` (`[default: 32]`), and its help ends with `Exit codes: 0 deleted, 1 error, 2 usage error, 3 conflict`.
- `reset` lists `--namespace`, `--collection`, `--root` and `--ancestor-cap`, and its help ends with `Exit codes: 0 no conflicts, 1 error, 2 usage error, 3 one or more conflicts`.
- `collect` lists `--older-than`, `--namespace`, `--collection`, `--root` and `--ancestor-cap`, and its help ends with `Exit codes: 0 success (conflicts are reported, not failures), 1 error, 2 usage error`.
- `sync` lists `--ancestor-cap`.

Any difference is a slice 6 bug: stop and report it rather than changing the docs.

- [ ] **Step 2: Write the guide page**

Create `docs/guide/src/deletion.md` with exactly:

````markdown
# Deleting records, resetting namespaces, and collecting tombstones

Deleting a record in gonzalo is a **replicated write**. When you delete a record,
the store keeps a small marker, a *tombstone*, where the record was. The tombstone
travels to other stores through `sync` and git `pull` like any other change, so a
delete made on one machine takes effect everywhere and stays in effect.

Up to 0.6, deletion only removed the record from the store you ran it on, and the
next sync with a peer that still had the record brought it back. That no longer
happens. The design is recorded in
[ADR 0021](./adr/0021-replicated-deletion-with-tombstones.md).

## Upgrade every binary together

> **Warning.** 0.7 adds a record kind that older binaries cannot read. Upgrade
> every gonzalo binary that touches a store, and every program built on
> `gonzalo-core`, **at the same time.**
>
> - A 0.6 binary that reads a store holding a tombstone fails on that key with a
>   serialization error. This is loud, and no data is lost.
> - A 0.6 binary that runs **sync** against 0.7 data can't see tombstones and
>   copies deleted records back. This is **silent**, and nothing on the new side
>   can detect or block it.
> - A 0.7 client talking to a 0.6 `gonzalod` still gets, puts, lists and deletes
>   normally. Replication reads fail with
>   `daemon predates replication reads (gonzalo#203); upgrade gonzalod` instead
>   of quietly doing the wrong thing.

## Two kinds of reads

Everything an application does (`gonzalo list`, `gonzalo get`, the MCP server,
memory tiers, tickets) uses **consumer reads**. Consumer reads hide tombstones, so
a deleted record looks exactly like one that never existed.

Replication uses **raw reads**, which show tombstones, and **raw writes**, which
store a record exactly as given. `sync`, git `pull` and `gonzalo collect` need to
see deletes in order to propagate or clean them up. You won't normally use either
yourself. On the daemon they are separate routes
(see [Over the daemon](#over-the-daemon)).

The `gonzalo delete`, `reset` and `collect` commands below work on a local store
directory given by `--root`, which defaults to the current directory. For a store
behind `gonzalod`, use the daemon's routes.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success. For `collect`, conflicts are reported but aren't failures |
| `1` | Error: I/O, the store, or `--ancestor-cap 0` (`ancestor cap must be at least 1`) |
| `2` | Usage error: a missing or malformed argument |
| `3` | Conflict (`delete` and `reset` only): someone changed a record you were deleting. Re-read, or re-run the reset |

Every command takes `--root <DIR>` (default `.`, a leading `~` is expanded) and
`--ancestor-cap <K>` (see [Ancestor cap](#ancestor-cap)). Each command's `--help`
ends with its exit codes.

## Deleting a record

```sh
gonzalo delete --root ~/.gonzalo --namespace notes --collection topics --id rust-async
```

```text
deleted: notes/topics/rust-async
```

The command exits `0` when the record is deleted, and also when it was already
deleted or never existed: deleting is idempotent.

Pass `--expected` to delete only if nobody has changed the record since you read
it. The value is the revision as JSON, exactly as `gonzalo get` prints it.
Anything that isn't valid revision JSON is a usage error (exit `2`):

```sh
gonzalo delete --root ~/.gonzalo --namespace notes --collection topics --id rust-async \
  --expected '{"counter":3,"hash":"9f2c…"}'
```

If someone has changed the record, nothing is deleted, and the command prints the
current revision and exits `3`:

```text
conflict: notes/topics/rust-async
current:  {"counter":4,"hash":"b71e…"}
```

A tombstone names who deleted the record as its author. `gonzalo delete` records
the author `gonzalo-cli`. Through the daemon, the author is the authenticated
principal that made the request. Tombstones written by `gonzalo reset` carry no
author of their own.

Deleting a key this store has never seen writes nothing. If a peer holds a record
you want gone, delete it on that peer, or sync first and then delete.

### Delete versus a concurrent edit

If you delete a record on one machine while someone edits the same version on
another, sync can't know which intent should win. It reports a **conflict** for
that key and leaves both sides as they are, the same way it reports any edit it
can't merge. Resolve it by deleting again or rewriting the record on one side,
then sync.

### Recreating a deleted record

Writing a record to a deleted key simply recreates it, and the new record wins
over the old delete everywhere it syncs. Gonzalo sets the new record's revision so
that it comes after the delete. Programs writing through the API should use the
revision the write returns, not the one they built themselves. A conditional write
that names a revision to a deleted key fails with "not found", because to an
application the key doesn't exist. Write it unconditionally to recreate it.

## Resetting a namespace

`reset` deletes every live record in a namespace, or in one collection of it:

```sh
gonzalo reset --root ~/.gonzalo --namespace scratch
gonzalo reset --root ~/.gonzalo --namespace scratch --collection sessions
```

It prints a summary such as `12 deleted, 0 conflicts` and exits `0`. If any record
was edited during the reset, it prints one `conflict: <ns/col/id>` line per record
to stderr and exits `3`. Run it again to finish. `--namespace` is required, and leaving it out is a usage
error (exit `2`). Resetting every namespace at once isn't offered; loop over
namespaces if that's really what you want.

Reset is **not atomic**. It deletes records one at a time, and a record someone
edits during the reset is reported as a conflict and left alone. Reset is
**safe to re-run**: a second run deletes what the first missed and skips what's
already gone. Because reset is made of ordinary deletes, it replicates like them,
and needs only write access to the namespace.

## Collecting tombstones

Tombstones are small, but they stay until you remove them. `collect` physically
removes tombstones older than a horizon you choose:

```sh
gonzalo collect --root ~/.gonzalo --older-than 90d
gonzalo collect --root ~/.gonzalo --older-than 90d --namespace scratch
```

`--older-than` is required and has no default. It takes a positive integer and
exactly one unit, `d`, `h`, `m` or `s` (`90d`, `36h`). Zero, a missing unit, a
compound value such as `1d12h`, an unknown unit, or a value too large are all usage
errors (exit `2`). The command prints the horizon as you typed it, with its length
in seconds, then how many tombstones it purged, how many it kept because they carry
no deletion time, and how many conflicted:

```text
horizon:   90d (7776000s)
purged:    41
unstamped: 0
conflicts: 0
```

A conflict means the key was recreated while collection ran, and the new record is
left intact. Each conflict is also printed as a `conflict: <key>` line on stderr.
`collect` still exits `0` with conflicts, since nothing needs retrying. Without
`--namespace`, it runs across the whole store. `--collection` without
`--namespace` is a usage error (exit `2`).

There is **no default horizon**, and nothing collects automatically. That is
deliberate, and the next section explains why.

Collection only frees space in the store you run it on. If a peer still holds the
tombstone, the next sync copies it back. That's harmless (it's still a delete),
but to reclaim space everywhere, run `collect` on every store with the same
horizon.

Blobs referenced by deleted records aren't removed by collection. Blobs are
shared between records by content, so removing one safely needs a separate blob
garbage collector, which doesn't exist yet.

## Choosing a collection horizon

**This is the one decision in this page that can lose data.**

A tombstone is the only thing that stops a deleted record coming back. Suppose
you collect a tombstone, and later a peer that was offline the whole time syncs.
That peer still holds the live record and never saw the delete. With the
tombstone gone, sync treats the peer's copy as a record this store has never seen
and copies it back. The record is resurrected, on a different machine, possibly
weeks later, with no error anywhere.

So the rule is:

> **The horizon must be longer than the longest time any peer might go without
> syncing, plus a safety margin.**

Gonzalo can't know how long your peers stay offline, which is why you have to say.

| Your setup | Longest realistic gap between syncs | Suggested `--older-than` |
|---|---|---|
| One store, never synced with anything | none | any, e.g. `7d` |
| A few machines that sync with a daemon daily | a long weekend, a holiday | `30d`–`60d` |
| Laptops that can be offline for weeks (travel, leave) | several weeks | `90d` or more |
| Peers you don't control, or you don't know | unknown | don't collect, or `180d`+ |

Practical advice:

- **When in doubt, go longer.** A horizon that's too long costs a little storage.
  One that's too short costs data, silently, later.
- **Clocks.** The horizon is measured against each tombstone's deletion time. A
  machine whose clock was ahead produces tombstones that look younger than they
  are, so they're collected *later*, never earlier. Clock skew can delay
  collection but can't trigger it early.
- **Log the output.** If you run `collect` from a scheduler, keep its output. It
  records the horizon used, which is what you'll want to know if a record ever
  reappears.
- **Retiring a peer?** Sync it one last time before you drop it, so its view of
  deletes is current. A peer that is gone for good can't resurrect anything.

## Ancestor cap

Each record keeps a short list of the revisions it came from, so sync can tell
"this side is just behind" apart from "both sides changed". The list is capped at
32 entries by default. To change it, pass `--ancestor-cap <n>` to `gonzalo delete`,
`reset`, `collect` or `sync`, or set the `GONZALO_ANCESTOR_CAP` environment
variable for `gonzalod`, which has no command-line flags. The cap must be at least
1. Most deployments never need to change it.

A record edited more times than the cap since two stores last synced is treated as
changed on both sides: sync merges it or reports a conflict, but never silently
overwrites. Peers with different caps work together correctly.

## Over the daemon

`gonzalod` serves the same operations. Existing record routes keep their
meaning, with tombstones hidden:

| Operation | HTTP | Permission |
|---|---|---|
| Delete (writes a tombstone authored by the caller) | `DELETE /v1/records/{ns}/{col}/{id}` | `write` on the namespace |
| Raw read of one record | `GET /v1/raw/records/{ns}/{col}/{id}` | `read` on the namespace |
| Raw write (replication; revision stored verbatim, never re-stamped) | `PUT /v1/raw/records/{ns}/{col}/{id}` | `write` on the namespace |
| Raw key listing | `GET /v1/raw/keys?namespace=&collection=` | `read` on the namespace; admin without `namespace` |
| Purge (physical removal) | `POST /v1/purge/{ns}/{col}/{id}` with the expected revision as the JSON body | admin |

gRPC has the matching `GetRaw`, `ListRaw`, `PutRaw` and `Purge` RPCs, with the same
permissions.

**Who a replicated record is attributed to.** A raw write never changes a record's
revision, but its author follows the same rule as a normal write. If the caller is
not an admin, the daemon sets the record's author to the caller, so nobody can plant
a record in someone else's name through the raw route. If the caller is an admin, or
the daemon runs without auth, the record keeps the author it arrived with. Use an
admin token for replication when authorship must carry across stores.

**Writes to a deleted key.** A conditional write (normal or raw) whose expected
revision the daemon rejects as not found returns **HTTP 412**
(gRPC `FailedPrecondition`), and the Rust client reports it as not found. A
conditional normal write over a deleted key is the common case. This used to be an
opaque HTTP 500. It is not a 404, because on the raw routes a 404 means the daemon
is too old to have them.

Purge requires admin because it is the one operation that can make a
delete undoable across peers.
````

- [ ] **Step 3: Add the page to the guide summary**

In `docs/guide/src/SUMMARY.md`, replace:

```markdown
# Guides

- [The MCP server](./mcp.md)
```

with:

```markdown
# Guides

- [The MCP server](./mcp.md)
- [Deletion, reset & collection](./deletion.md)
```

- [ ] **Step 4: Update the stale parity-matrix delete row**

In `docs/evaluation/competitors/mem0/parity-gap-matrix.md`, replace:

```markdown
| Delete (`delete`) | ✅ | `Store::delete`, OCC-aware, propagated by `Sync` (ADR 0018) |
```

with:

```markdown
| Delete (`delete`) | ✅ | `Store::delete`, OCC-aware, replicated by `sync` and git `pull` as a tombstone; namespace `reset` and explicit tombstone `collect` (ADR 0018, ADR 0021) |
```

- [ ] **Step 5: Build the guide**

mdBook is installed locally (`command -v mdbook` shows `~/.cargo/bin/mdbook`). If `command -v mdbook` prints nothing, skip to the commit and rely on the `docs` workflow, which runs these same scripts with mdBook 0.4.52.

```bash
command -v mdbook
./docs/guide/sync-adrs.sh
./docs/guide/sync-changelog.sh
mdbook build docs/guide
```

Expected: `sync-adrs.sh` prints nothing, and `mdbook build` ends with `INFO HTML book written to .../docs/guide/book` and no `ERROR` lines. Then:

```bash
rg -n "deletion.md|0021-replicated" docs/guide/src/SUMMARY.md
ls docs/guide/book/deletion.html
```

Expected: SUMMARY lists `deletion.md` and `ADR 0021 · Replicated deletion with tombstones`, and `deletion.html` exists. The `docs/guide/src/adr/`, `changelog.md` and `book/` outputs are gitignored.

`sync-adrs.sh` rewrote the block after `<!-- adrs -->` in `SUMMARY.md`. The tracked copy was stale and lacked ADR 0020, so the diff also adds 0020 and 0021 rows. Commit that regenerated block as is: it's what CI generates anyway.

- [ ] **Step 6: Commit**

```bash
git add docs/guide/src/deletion.md docs/guide/src/SUMMARY.md docs/evaluation/competitors/mem0/parity-gap-matrix.md
git commit -m "docs(guide): deletion, reset and collection, with horizon guidance

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 7: CHANGELOG

**Files:**
- Modify: `CHANGELOG.md:10` (`## [Unreleased]` section)

**Interfaces:**
- Consumes: the ADR 0021 filename (Task 5) and the CLI contract table in Task 6's Interfaces (copied from slice 6's plan).
- Produces: the `[Unreleased]` notes `cai-cut-release` turns into the 0.7.0 section.

- [ ] **Step 1: Check what slices 1–6 already added**

```bash
awk '/^## \[Unreleased\]/{f=1;next} /^## \[/{f=0} f' CHANGELOG.md
```

Expected: empty output, or entries from earlier slices. Keep any entry that is **not** about #203 tombstones, delete, reset, collect, raw reads, purge, sync ancestry or `--ancestor-cap`, and re-insert it verbatim under the matching heading in Step 2. Drop earlier-slice entries about those topics; Step 2's text replaces them.

- [ ] **Step 2: Write the section**

Replace everything between `## [Unreleased]` and `## [0.6.0] - 2026-09-10` (exclusive) with:

```markdown
## [Unreleased]

**Upgrade every gonzalo binary together.** Deletion now replicates, and it does
so with a record kind older binaries can't read. A 0.6 `gonzalo`, `gonzalod`, or
any program built on `gonzalo-core` 0.6 that reads a store holding a tombstone
fails on that key with a serialization error: loud, and no data is lost. The
silent failure is worse: a 0.6 binary that runs **sync** against 0.7 data can't see
tombstones and copies deleted records back, and nothing on the new side can tell
that apart from a genuine recreation. Upgrade every binary that reads a store
directly, and every binary that runs sync, at the same time. A 0.7 client
against a 0.6 `gonzalod` keeps working for normal reads and writes, and fails
replication reads with an explicit `upgrade gonzalod` error rather than falling
back. See the guide's "Deletion, reset & collection" page and ADR 0021. (#203)

### Added

- **Replicated deletion.** A delete writes a tombstone (`RecordKind::Tombstone`)
  at the record's own path, and `sync` and git `pull` propagate it, so a delete
  on one store sticks everywhere instead of coming back on the next sync.
  Tombstones carry `deleted_at` (ms since the Unix epoch), and records carry a
  bounded `ancestors` list. Both are optional fields, so live records written by
  0.7 still read on 0.6. See ADR 0021. (#203)
- **`gonzalo delete --namespace <N> --collection <C> --id <I> [--expected <REVISION_JSON>] [--root <DIR>] [--ancestor-cap <K>]`**:
  delete one record by writing a tombstone attributed to `gonzalo-cli`.
  - `--expected` is a revision as JSON, exactly as `gonzalo get` prints it.
  - On success, including an absent or already-deleted record, prints `deleted: N/C/I` and exits `0`.
  - On a stale `--expected`, prints `conflict: N/C/I` and `current:  {…}` and exits `3`.
  - Malformed `--expected` exits `2`.
  
  (#203)
- **`gonzalo reset --namespace <N> [--collection <C>] [--root <DIR>] [--ancestor-cap <K>]`**:
  tombstone every live record in a namespace or collection. It's not atomic but
  it's safe to re-run. Prints `X deleted, M conflicts`, plus one
  `conflict: <ns/col/id>` line per conflict on stderr. Exits `0` with no conflicts
  and `3` with any (re-run to finish). Omitting `--namespace` is a usage error
  (exit `2`). (#203)
- **`gonzalo collect --older-than <DURATION> [--namespace <N> [--collection <C>]] [--root <DIR>] [--ancestor-cap <K>]`**:
  physically purge tombstones older than a horizon **you** choose.
  - `<DURATION>` is a positive integer plus exactly one unit of `d`, `h`, `m` or `s` (e.g. `30d`), with no default. Zero, a missing, compound or unknown unit, or overflow exits `2`, as does `--collection` without `--namespace`.
  - It prints `horizon:`, `purged:`, `unstamped:` and `conflicts:` lines, and exits `0` even with conflicts.
  - Purging a tombstone before every peer has synced lets the deleted record come back from that peer.
  - Tombstones with no deletion time are never collected, and clock skew can only delay collection.
  - The guide has a section on choosing a horizon.
  
  (#203)
- **Common to `delete`, `reset` and `collect`:**
  - `--root <DIR>` (default `.`, expands a leading `~`) and `--ancestor-cap <K>` (default 32). `0` exits `1` with `ancestor cap must be at least 1`.
  - Exit codes: `0` success, `1` error, `2` usage error, `3` conflict (`delete` and `reset`). Each command's `--help` lists its codes.
  - The CLI operates on a local fs store only.
  
  (#203)
- **Replication surface on `Store`**: `get_raw` and `list_raw` show tombstones;
  `put_raw(record, expected)` stores a record with its revision verbatim and never
  re-stamps the revision (through the daemon, a non-admin caller still becomes the
  author, while admin and open mode keep the replicated author);
  `purge(key, expected)` is the only physical removal left. (#203)
- **`Store::delete_as(key, expected, author)`** records who deleted a record as
  the tombstone's author. The daemon passes the authenticated principal and
  `gonzalo delete` passes `gonzalo-cli`. The provided `delete`, which `reset` uses,
  passes none. (#203)
- **Daemon routes**:
  - `GET /v1/raw/records/{ns}/{col}/{id}` and `GET /v1/raw/keys` (`read` on the namespace; admin when unscoped);
  - `PUT /v1/raw/records/{ns}/{col}/{id}` (`write` on the namespace);
  - `POST /v1/purge/{ns}/{col}/{id}` (admin);
  - the matching `GetRaw`, `ListRaw`, `PutRaw` and `Purge` RPCs.
  
  (#203)
- **Ancestor cap**: `--ancestor-cap <n>` on `gonzalo delete`, `reset`, `collect`
  and `sync`, and the `GONZALO_ANCESTOR_CAP` environment variable on `gonzalod`,
  set how many prior revisions a record remembers. The default is 32 and 0 is
  rejected. Peers with different caps interoperate. (#203)

### Changed

- **Breaking: `gonzalo-core`'s `Store` trait gains required methods**
  (`get_raw`, `list_raw`, `put_raw`, `purge`, `delete_as`), `delete` becomes a
  provided method over `delete_as`, and `RecordKind` gains `Tombstone`, so this
  release is 0.7.0 across the workspace. The replication methods have no default
  implementation on purpose: `get_raw = get` would compile, pass every test that
  doesn't delete, and resurrect records in production. Third-party `Store`
  implementations get a compile error naming exactly what to add, and must
  implement `delete_as` instead of `delete`. (#203)
- **Sync copies one-sided records with `put_raw`.** A delete landing on the
  destination between sync's read and its copy now conflicts and is resolved on
  the next pass. Before, the copy became a recreation and brought the deleted
  record back. (#203)
- **s3 retries lost conditional writes.** A write that loses an `If-Match` race —
  `PreconditionFailed` (412), `ConditionalRequestConflict` (409), or `NoSuchKey`
  (the object vanished to a concurrent purge) — re-reads, re-plans and retries
  up to 8 times before returning a backend error, so s3 reaches the same
  outcomes as the lock-based fs and git stores. (#203)
- **`Store::delete` writes a tombstone instead of removing the record.** `get`
  and `list` hide tombstones, so applications see no difference. Deleting an
  already-deleted or absent key is still an idempotent `Deleted`. ADR 0018's
  local-only decision is superseded by ADR 0021, and its conflict semantics are
  unchanged. (#203)
- **Writing to a deleted key re-stamps the revision.** A `put` with no expected
  revision over a tombstone stores the record with a revision one past the
  delete, so the recreation wins over the delete when synced. `put` still returns
  the stored revision; a caller that ignores it and reuses its own will get a
  conflict on its next conditional write. A `put` with any expected revision over
  a deleted key returns `NotFound`. (#203)
- **The daemon returns 412 for a write rejected as not found.** A consumer
  `PUT /v1/records/{ns}/{col}/{id}` (or raw `PUT`) whose expected revision the store
  rejects with `NotFound`, such as a conditional write over a deleted key, now
  returns HTTP 412 (gRPC `FailedPrecondition`) instead of an opaque 500
  (`Internal`). `ServerStore` maps it back to `CoreError::NotFound`. 404 is
  deliberately not used, because on the raw routes it signals a daemon too old to
  serve them. (#203)
- **Raw writes through the daemon keep ADR 0015's authorship rule.** A `put_raw`
  from a non-admin principal has `meta.author` restamped to that principal, like a
  consumer `put`. An admin principal, or a daemon without auth, keeps the replicated
  record's author, so an admin token is the replication credential. The revision
  is never re-stamped in either case. (#203)
- **Sync fast-forwards when one side is simply behind.** Using each record's
  ancestors, sync now overwrites the stale side instead of running a merge.
  Before, an `Opaque` kind such as `Checkpoint` reported a conflict even though
  nothing had diverged. A delete racing an edit of the same revision is reported
  as a `SyncConflict` with neither side written. Records written before 0.7 have no
  ancestors and take the old merge path unchanged. (#203)
- **git `pull` handles tombstones** on paths changed on both sides: concurrent
  tombstones converge on the newer one, and delete-versus-edit is reported as a
  `PullConflict` that keeps local. (#203)

Testing: the HA soak races deletes against edits and recreations across
`gonzalod` replicas under replica-kill chaos, and checks that every replica
agrees on each key's deletion state, that consumer reads match raw reads, and
that acked deletes survive replica kills. The tombstone conformance cases run on
fs, git, s3 and the daemon client. (#203)

Docs: ADR 0021, a guide page for delete, reset and collect with horizon
guidance, and the ADR 0018 index back-reference. (#203)

```

(The last line of the block is intentionally blank, so one blank line separates it from `## [0.6.0] - 2026-09-10`.)

- [ ] **Step 3: Check the result renders and the version headers are intact**

```bash
rg -n "^## \[" CHANGELOG.md | head -3
./docs/guide/sync-changelog.sh
mdbook build docs/guide
```

Expected: the first three headers are `## [Unreleased]`, `## [0.6.0] - 2026-09-10` and `## [0.5.0] - 2026-08-22`, and `mdbook build` reports no `ERROR`. If mdBook isn't installed, run only the `rg`.

- [ ] **Step 4: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs(changelog): replicated deletion, reset, collect, upgrade warning

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 8: File follow-up issues

**Files:** none. This task creates GitHub issues only.

**Interfaces:**
- Produces: three issue URLs, recorded in shell variables and used in the Task 9 PR body.

- [ ] **Step 1: Confirm the labels exist**

```bash
gh label list --repo caliban-ai/gonzalo --limit 100 | rg '^(kind/feature|kind/design|area/core|area/store|area/performance)\s'
```

Expected: five lines, one each for `kind/feature`, `kind/design`, `area/core`, `area/store` and `area/performance`. Remove any label missing from this output from the commands below.

- [ ] **Step 2: Create the issues**

Run all three from one shell session so the variables persist into Task 9. Otherwise, copy the printed URLs into the PR body by hand.

```bash
BLOB_GC_URL=$(gh issue create --repo caliban-ai/gonzalo \
  --title "Garbage-collect orphaned blobs left by deleted records" \
  --label "kind/feature,area/core,area/store" \
  --body "$(cat <<'EOF'
## Problem

Deleting a record whose body is `Body::Blob` leaves the blob in the `BlobStore`. Purging the tombstone later (`gonzalo collect`) removes the record but still not the blob. Blobs are content-addressed and can be shared between records (ADR 0012), so `purge` can't safely delete them. Storage for blob-backed records is never reclaimed.

## Scope

A mark-and-sweep over blobs: mark every blob referenced by any live record **or tombstone's history that can still be recreated from a peer** (decide whether tombstones pin blobs), and sweep the rest. `gc_blobs` already exists for code-graph slices and is a starting point. It must be explicit and operator-run, like `collect`, for the same reason: a blob swept too early can't be restored by a later sync.

## References

- ADR 0021, Consequences (orphaned blobs)
- `docs/superpowers/specs/2026-09-13-tombstone-replication-design.md` §8.3
- Follow-up from #203
EOF
)")
echo "$BLOB_GC_URL"

META_TIMES_URL=$(gh issue create --repo caliban-ai/gonzalo \
  --title "Populate Meta.created and Meta.updated on writes" \
  --label "kind/feature,area/core" \
  --body "$(cat <<'EOF'
## Problem

`Meta.created` and `Meta.updated` are always `0` today: no store or client stamps them. Tombstones added a separate `deleted_at` stamped by the store, which made the gap more visible, since operators can see when a record was deleted but not when it was created or last changed.

## Scope

Decide who stamps the times (the store inside its OCC critical section, like `deleted_at`, or the daemon, like `meta.author`), and what replication preserves (sync must copy the source's times, not restamp). Recreation over a tombstone should reset `created`. Add conformance cases so every substrate agrees.

## References

- `docs/superpowers/specs/2026-09-13-tombstone-replication-design.md` §1.3 (out of scope)
- ADR 0021, Consequences
- Follow-up from #203
EOF
)")
echo "$META_TIMES_URL"

S3_LIST_URL=$(gh issue create --repo caliban-ai/gonzalo \
  --title "s3: consumer list issues a GetObject per key to filter tombstones" \
  --label "area/performance,area/store,kind/design" \
  --body "$(cat <<'EOF'
## Problem

Since tombstones (#203), consumer `list` has to exclude tombstoned keys, and a tombstone is only recognisable by reading the record. On fs and git that's a local read per key. On s3 it's a `GetObject` per key on top of `ListObjectsV2`, which is slow and costly for large namespaces. `reset` and `collect` inherit the cost.

## Candidate fixes (each is a layout change and needs its own design + ADR)

- Encode the record kind in the object key (e.g. a tombstone suffix), so `list` filters on key names alone. The rename must stay atomic with the conditional write.
- A per-collection tombstone index object, conditionally updated alongside each delete and purge.

## Acceptance

A benchmark over a namespace with N live records and M tombstones on RustFS, before and after, and conformance still passing.

## References

- ADR 0021, Consequences and Revisit if
- `docs/superpowers/specs/2026-09-13-tombstone-replication-design.md` §8.4
EOF
)")
echo "$S3_LIST_URL"
```

Expected: three `https://github.com/caliban-ai/gonzalo/issues/<n>` URLs printed.

- [ ] **Step 3: Verify**

Run: `gh issue list --repo caliban-ai/gonzalo --search "Garbage-collect orphaned blobs OR Populate Meta.created OR consumer list issues a GetObject" --state open`
Expected: the three new issues listed.

---

### Task 9: Full gate, push, and PR

**Files:** none.

- [ ] **Step 1: Run the verification gate (bare commands, one per line)**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-features
```

Expected: `fmt` prints nothing and exits 0. `clippy` ends with `Finished` and no `warning:` lines. `build` ends with `Finished`. `test` has every `test result:` line `ok` and no `FAILED`; `ha_soak_bounded` passes by skipping unless an S3 target is exported. If `fmt --check` fails, run `cargo fmt --all`, commit the result as `style: cargo fmt`, and re-run all four.

- [ ] **Step 2: Docs checks**

```bash
./docs/guide/sync-adrs.sh
./docs/guide/sync-changelog.sh
mdbook build docs/guide
git status --short
```

Expected: `mdbook build` reports no `ERROR`, and `git status --short` is empty. The regenerated SUMMARY was committed in Task 6; the other outputs are gitignored. If `SUMMARY.md` shows as modified, commit it as `docs(guide): regenerate ADR summary`.

- [ ] **Step 3: Validate the ADR log once more**

Invoke the `adr-validate` skill on `docs/adr/`. Expected: all six checks PASS, as in Task 5 Step 5. Don't push with any finding open.

- [ ] **Step 4: Push**

```bash
git push -u origin feat/203-tombstones-07-soak-and-docs
```

Expected: `branch 'feat/203-tombstones-07-soak-and-docs' set up to track 'origin/feat/203-tombstones-07-soak-and-docs'`.

- [ ] **Step 5: Open the PR**

```bash
gh pr create --repo caliban-ai/gonzalo \
  --base main \
  --head feat/203-tombstones-07-soak-and-docs \
  --title "Tombstones slice 7: soak deletion invariants and docs" \
  --body "$(cat <<EOF
Part of #203

Final slice of replicated deletion (spec \`docs/superpowers/specs/2026-09-13-tombstone-replication-design.md\` §6.7, §7 item 8).

## Soak
- Writers interleave edit / delete / recreate ops on \`life-*\` keys and delete some acked unique keys.
- After the writers stop and the killed replica is back, the harness reads every lifecycle key from **every** replica (\`get_raw\` + \`get\`). The oracle requires live-everywhere or tombstone-everywhere, identical revisions, consumer reads matching raw reads, no committed key physically absent, acked deletes still tombstones, and at least one delete conflict.
- Honest scope: the soak's replicas share one bucket, so this catches daemon read-path and filtering bugs and resurrection under churn, not storage replication. Sync and pull are covered by slice 5's tests. The oracle is topology-agnostic.
- \`ha-soak\` now also triggers on \`gonzalo-core\` and \`gonzalo-store-server\` changes.

## Docs
- ADR 0021 *Replicated deletion with tombstones* (accepted). ADR 0018 stays accepted with \`local-only deletion superseded by 0021\` in the index; its body is untouched. adr-validate passes.
- Guide page: delete, reset, collect, raw vs consumer reads, upgrade-together warning, and **Choosing a collection horizon**.
- CHANGELOG \`[Unreleased]\`: Added / Changed plus the upgrade warning.

## Follow-ups filed
- ${BLOB_GC_URL}
- ${META_TIMES_URL}
- ${S3_LIST_URL}

## Verification
- fmt, clippy -D warnings, build, test (all-features): green locally
- mdbook build: clean
- RustFS bounded gate: see the ha-soak check on this PR

https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
)"
```

Expected: a PR URL is printed. If the Task 8 variables are empty because this is a different shell, replace the three `${...}` lines with the URLs Task 8 printed before running the command.

- [ ] **Step 6: Wait for CI and merge**

```bash
gh pr checks --watch
```

Expected: `ci / fmt · clippy · build · test`, `coverage`, `package-check`, `docs / build` and `ha-soak` all pass. Then merge:

```bash
gh pr merge --squash --delete-branch
```

If `ha-soak` fails, fetch the logs with `gh run view --log-failed` and fix the underlying cause before merging. Don't retry blindly: a `MixedLiveAndTombstone`, `ConsumerRawMismatch` or `AckedDeleteLost` there is a real bug in slices 1–4.

---

## Self-Review

**Spec coverage:**
- §6.7 Delete/Recreate ops mixed into concurrent writers: Task 3 (`lifecycle_step`, interleaved in `writer`).
- §6.7 convergence invariant: Task 1 (`MixedLiveAndTombstone`, plus the stronger `ReplicasDisagree`, `ConsumerRawMismatch`, `LifecycleKeyVanished`); per-replica collection in Tasks 3–4.
- §6.7 conflict-exercised check extended to deletes: Task 1 (`NoDeleteConflictsObserved`, `NoDeletesCommitted`).
- §4.3 ADR 0021 and the 0018 index row: Task 5. Rejected alternatives covered: local-only reset, hybrid, counter ordering, version vectors, automatic collection, separate tombstone index.
- §7 item 8 guide pages with a prominent horizon section: Task 6. The CHANGELOG upgrade warning (§4.1, §8.1): Task 7.
- §7 follow-up tickets (§8.3, `Meta` times, §8.4): Task 8.
- §8.2 horizon guidance: Task 6. §8.5 recreation re-stamp: guide and CHANGELOG.

**Reconciled contract coverage:** `put_raw` and the resurrection window (ADR 0021 Decision, guide daemon table, CHANGELOG); `NotFound` for a consumer put with `Some` over a tombstone (ADR, guide, CHANGELOG, soak lost-race mapping); `delete_as` and authorship (ADR, guide, CHANGELOG, soak test doubles); the `put_raw` author rule, non-admin restamped and admin/open kept (ADR, guide daemon section, CHANGELOG); daemon `NotFound` as 412 / `FailedPrecondition` (ADR, guide daemon section, CHANGELOG Changed); s3's 8-attempt lost-race loop, 412/409/NoSuchKey (ADR, CHANGELOG); `GONZALO_ANCESTOR_CAP` as env var only (ADR, guide, CHANGELOG); the CLI synopsis and exit codes 0/1/2/3 copied from slice 6's Task 4 table (guide, CHANGELOG, ADR exit-code paragraph). The soak has no replication writes, so it uses no `put_raw`.

**Placeholder scan:** no TBD/TODO. The only conditional edit is Task 7 Step 1 (earlier-slice CHANGELOG entries). Task 6 Step 1 compares `--help` output with the fixed synopsis and treats any difference as a slice 6 bug.

**Type consistency:** `LifecycleOp`, `LifecycleResult`, `LifecycleRecord`, `RawState`, `ReplicaView`, `FinalLifecycle`, `delete_conflicts`, `collect_lifecycle`, `lifecycle_op_for`, `lifecycle_key_for`, `raw_state`, `Dispatcher::{delete,get_raw,replicas}` and the three new `WorkloadConfig` fields are spelled identically in Tasks 1–4. Contract names used: `Record::is_tombstone`, `tombstone_of(&Record, i64, usize, Option<&Identity>)`, `DEFAULT_ANCESTOR_CAP`, `Store::{get_raw, list_raw, put_raw, purge, delete_as}` (with `delete` provided), `Record { ancestors, deleted_at }`.
