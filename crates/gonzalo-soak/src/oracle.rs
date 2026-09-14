//! The soak safety/liveness oracle.
//!
//! Given the recorded outcome of every write op plus a final read of every key,
//! [`check`] asserts the invariants a correct conditional-write store must hold
//! under concurrent multi-replica load and replica-kill chaos:
//!
//! - **No lost update** — every *committed* op-id on a contended key survives in
//!   that key's final record, exactly once.
//! - **Conflicts surface** — racing writers observed `Conflict` (never a silent
//!   overwrite). Zero observed conflicts means the invariant was never actually
//!   exercised, which is itself a failure. The same holds for deletes racing
//!   edits: zero delete conflicts is a failure.
//! - **One commit per base** — on a lifecycle key, two edits never both commit
//!   on the same base revision, and an edit never commits on the same base as a
//!   delete that actually wrote a tombstone there. A delete acknowledged without
//!   evidence that it wrote (a no-op over a later tombstone, or a raced evidence
//!   read) is ambiguous and skipped, never flagged.
//! - **Durability under churn** — every acked unique-key write is still readable,
//!   and every acked delete still reads as a tombstone (no resurrection).
//! - **Replicas agree on deletion** (#203) — once the writers have stopped and every
//!   replica is live again, each lifecycle key is live on every replica or a
//!   tombstone on every replica (never a mix), every replica reports the same raw
//!   revision, each replica's consumer read agrees with its own raw read on
//!   whether the key is live, and a key that had committed writes is never
//!   physically absent.
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
//! Per-replica agreement is checked for the `life-*` keys; deleted unique keys
//! are checked through one replica, because they use the same read path.
//!
//! This is a targeted invariant oracle, not a linearizability checker: it asserts
//! on set membership / agreement / completion, never on exact interleavings,
//! so normal scheduling jitter cannot flake it.

use std::collections::{BTreeMap, BTreeSet};

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
    /// A raw read returned a tombstone. Only meaningful when `deleted`. `false`
    /// also covers a failed raw read (`get_raw` returning `Err`) — the collector
    /// cannot distinguish "not a tombstone" from "couldn't tell". As of this
    /// writing, `workload::collect_unique` folds `Err` into `false` via
    /// `matches!(store.get_raw(&key).await, Ok(Some(rec)) if rec.is_tombstone())`,
    /// so a raw read that errors on every replica during collection reads here
    /// as a lost delete rather than a read failure.
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
    /// A concurrent writer moved the key between the read and the write. A
    /// failover retry after a lost acknowledgement is also recorded here, which
    /// errs safe.
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
    /// The `"<counter>:<hash>"` rendering of the revision this op read and
    /// conditioned its write on. `None` for `Recreate`, and for any op that
    /// never read a live record (its `get` returned absent, so it wrote
    /// unconditionally or was skipped).
    pub base: Option<String>,
    /// For an acknowledged conditional `Delete`, whether it actually wrote the
    /// tombstone on its base: `Some(true)` when a raw read straight after the
    /// delete found a tombstone whose parent is `base`, `Some(false)` when it
    /// found a tombstone with a different parent (a no-op over an existing
    /// tombstone), and `None` when that read failed or found the key live or
    /// absent (a later op raced it). `None` for every other op and result.
    pub delete_wrote: Option<bool>,
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
    /// A replica's consumer read disagrees with its own raw read on whether the
    /// key is live: a tombstone served to consumers, or a live record hidden
    /// from them. Checked on liveness only, not on revision — see the module
    /// doc.
    ConsumerRawMismatch { key: String, replica: usize },
    /// A replica could not be read after settling.
    FinalReadFailed { key: String, replica: usize },
    /// A lifecycle key with committed writes reads as absent — delete must write
    /// a tombstone, never physically remove.
    LifecycleKeyVanished { key: String },
    /// Lifecycle ops ran but no delete ever lost a race — the delete-conflict
    /// invariant was not exercised.
    NoDeleteConflictsObserved,
    /// Lifecycle ops ran but no delete was ever acknowledged (a no-op delete
    /// over an existing tombstone counts).
    NoDeletesCommitted,
    /// Lifecycle ops ran but no per-replica final state was collected.
    LifecycleNotChecked,
    /// An acked delete of a unique key reads as live again, or not as a tombstone.
    AckedDeleteLost { key: String },
    /// Two conditional writes committed on the same base revision of one key:
    /// (a) two edits, or (b) an edit plus a delete that actually wrote a
    /// tombstone on that base (`delete_wrote == Some(true)`). Under OCC at most
    /// one can win, so this is a lost update or a resurrection over a tombstone
    /// (spec §3.2, §5.4). An edit sharing a base only with deletes whose write
    /// evidence is `Some(false)` or `None` is ambiguous (a stale delete can land
    /// as a no-op over a later tombstone and still report `Deleted`) and is
    /// skipped, never flagged.
    StaleBaseCommitted { key: String, base: String },
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

    // Under OCC at most one conditional write may commit on a given base
    // revision. Group committed ops by (key, base), skipping ops that never
    // read a live record, and flag a group that holds (a) two or more Edits —
    // `put` reports Committed only when it wrote — or (b) an Edit plus a Delete
    // with evidence that it wrote its tombstone on that base. Either is a lost
    // update or a resurrection over a tombstone. Deletes alone are legal: a
    // second delete on the same base is a no-op that still reports `Deleted`.
    // An Edit alongside only Deletes without write evidence is ambiguous: a
    // stale delete that reaches a later tombstone returns `Deleted` without
    // writing (spec §3.2), so it is skipped rather than flagged. A BTreeMap
    // keeps the emitted order deterministic.
    #[derive(Default)]
    struct BaseGroup {
        edits: usize,
        deletes_that_wrote: usize,
    }
    let mut committed_by_base: BTreeMap<(&str, &str), BaseGroup> = BTreeMap::new();
    for o in &stats.lifecycle_ops {
        if o.result != LifecycleResult::Committed {
            continue;
        }
        let Some(base) = o.base.as_deref() else {
            continue;
        };
        let group = committed_by_base.entry((o.key.as_str(), base)).or_default();
        match o.op {
            LifecycleOp::Edit => group.edits += 1,
            LifecycleOp::Delete if o.delete_wrote == Some(true) => group.deletes_that_wrote += 1,
            LifecycleOp::Delete | LifecycleOp::Recreate => {}
        }
    }
    for ((key, base), group) in &committed_by_base {
        if group.edits >= 2 || (group.edits >= 1 && group.deletes_that_wrote >= 1) {
            out.push(Violation::StaleBaseCommitted {
                key: (*key).to_string(),
                base: (*base).to_string(),
            });
        }
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

    fn lop(
        key: &str,
        op_id: u64,
        op: LifecycleOp,
        result: LifecycleResult,
        base: Option<&str>,
    ) -> LifecycleRecord {
        LifecycleRecord {
            key: key.into(),
            op_id,
            op,
            result,
            base: base.map(String::from),
            delete_wrote: None,
        }
    }

    /// `rec` with its delete write evidence set.
    fn wrote(mut rec: LifecycleRecord, delete_wrote: Option<bool>) -> LifecycleRecord {
        rec.delete_wrote = delete_wrote;
        rec
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
                lop(
                    "life-0",
                    0,
                    LifecycleOp::Recreate,
                    LifecycleResult::Committed,
                    None,
                ),
                wrote(
                    lop(
                        "life-0",
                        1,
                        LifecycleOp::Delete,
                        LifecycleResult::Committed,
                        Some("0:aaa"),
                    ),
                    Some(true),
                ),
                lop(
                    "life-0",
                    2,
                    LifecycleOp::Delete,
                    LifecycleResult::Conflict,
                    Some("0:bbb"),
                ),
                lop(
                    "life-1",
                    3,
                    LifecycleOp::Recreate,
                    LifecycleResult::Committed,
                    None,
                ),
                lop(
                    "life-1",
                    4,
                    LifecycleOp::Edit,
                    LifecycleResult::Skipped,
                    None,
                ),
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
        let mut s = clean_stats();
        s.unique[1].raw_tombstone = false; // a lost delete…
        let v = check(&s);
        assert!(v.contains(&Violation::AckedDeleteLost { key: "u2".into() }));
        assert!(
            !v.contains(&Violation::UniqueWriteLost { key: "u2".into() }),
            "…is reported once, not also as a lost write: {v:?}"
        );
    }

    #[test]
    fn detects_acked_delete_lost_when_readable_again() {
        let mut s = clean_stats();
        s.unique[1].readable_with_value = true; // resurrected
        assert_eq!(
            check(&s),
            vec![Violation::AckedDeleteLost { key: "u2".into() }]
        );
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
        assert_eq!(
            check(&s),
            vec![Violation::ReplicasDisagree {
                key: "life-0".into(),
                states: vec![
                    RawState::Tombstone {
                        revision: "1:t".into()
                    },
                    RawState::Tombstone {
                        revision: "2:t".into()
                    },
                    RawState::Tombstone {
                        revision: "1:t".into()
                    },
                ],
            }],
            "two tombstones at different revisions must be flagged, exactly once"
        );
    }

    #[test]
    fn detects_absent_on_one_replica_as_disagreement_and_vanished() {
        let mut s = clean_stats();
        s.lifecycle[1].replicas[0] = ReplicaView {
            raw: RawState::Absent,
            consumer_live: Some(false),
        };
        assert_eq!(
            check(&s),
            vec![
                Violation::ReplicasDisagree {
                    key: "life-1".into(),
                    states: vec![
                        RawState::Absent,
                        RawState::Live {
                            revision: "0:h".into()
                        },
                        RawState::Live {
                            revision: "0:h".into()
                        },
                    ],
                },
                Violation::LifecycleKeyVanished {
                    key: "life-1".into()
                },
            ]
        );
    }

    /// The #203-shaped read-path bug — a deleted key is a tombstone on some
    /// replicas but physically absent on another. Must be pinned as exactly
    /// `ReplicasDisagree` plus `LifecycleKeyVanished`, not `MixedLiveAndTombstone`
    /// (absent is not live).
    #[test]
    fn detects_tombstone_on_some_replicas_absent_on_another() {
        let mut s = clean_stats();
        s.lifecycle[0].replicas[2] = ReplicaView {
            raw: RawState::Absent,
            consumer_live: Some(false),
        };
        assert_eq!(
            check(&s),
            vec![
                Violation::ReplicasDisagree {
                    key: "life-0".into(),
                    states: vec![
                        RawState::Tombstone {
                            revision: "1:t".into()
                        },
                        RawState::Tombstone {
                            revision: "1:t".into()
                        },
                        RawState::Absent,
                    ],
                },
                Violation::LifecycleKeyVanished {
                    key: "life-0".into()
                },
            ]
        );
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
        assert_eq!(
            check(&s),
            vec![Violation::LifecycleKeyVanished {
                key: "life-0".into()
            }],
            "a committed-then-deleted key must be a tombstone, never physically gone"
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
        assert_eq!(
            check(&s),
            vec![],
            "no committed op on life-2, so absent is fine"
        );
    }

    #[test]
    fn detects_consumer_raw_mismatch() {
        let mut s = clean_stats();
        // Replica 1 serves a tombstoned key to consumers.
        s.lifecycle[0].replicas[1].consumer_live = Some(true);
        assert_eq!(
            check(&s),
            vec![Violation::ConsumerRawMismatch {
                key: "life-0".into(),
                replica: 1
            }]
        );
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
            Some("9:ccc"),
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

    /// A committed Edit and a committed Delete that wrote its tombstone share
    /// one base: the edit was wrongly accepted over the tombstone (a
    /// resurrection), or the delete wrongly overwrote the edit (a lost update).
    /// `clean_stats`'s life-0 already has a committed Delete on base "0:aaa"
    /// with `delete_wrote: Some(true)`; adding one committed Edit on the same
    /// base is the whole mutation.
    #[test]
    fn delete_that_wrote_on_an_edited_base_is_flagged() {
        let mut s = clean_stats();
        s.lifecycle_ops.push(lop(
            "life-0",
            100,
            LifecycleOp::Edit,
            LifecycleResult::Committed,
            Some("0:aaa"),
        ));
        assert_eq!(
            check(&s),
            vec![Violation::StaleBaseCommitted {
                key: "life-0".into(),
                base: "0:aaa".into(),
            }]
        );
    }

    /// The legal interleaving on one key at live revision r: W1 reads r to
    /// delete it; W2's edit r → r1 commits; W3 deletes r1 and writes tombstone
    /// t; W1's stale delete on r then reaches t, writes nothing and still
    /// reports `Deleted`. W1 and W2 both commit on base r, but the write
    /// evidence shows W1 was a no-op, so nothing is flagged.
    #[test]
    fn stale_delete_noop_over_later_tombstone_is_not_flagged() {
        let mut s = clean_stats();
        s.lifecycle_ops.push(lop(
            "life-1",
            110,
            LifecycleOp::Edit,
            LifecycleResult::Committed,
            Some("1:r"),
        ));
        s.lifecycle_ops.push(wrote(
            lop(
                "life-1",
                111,
                LifecycleOp::Delete,
                LifecycleResult::Committed,
                Some("2:r1"),
            ),
            Some(true),
        ));
        s.lifecycle_ops.push(wrote(
            lop(
                "life-1",
                112,
                LifecycleOp::Delete,
                LifecycleResult::Committed,
                Some("1:r"),
            ),
            Some(false),
        ));
        assert_eq!(check(&s), vec![]);
    }

    /// An Edit and a Delete committed on one base, where the delete's write
    /// evidence couldn't be taken (the raw read failed or found the key live
    /// or absent): ambiguous, so skipped rather than flagged.
    #[test]
    fn delete_with_unknown_write_evidence_is_not_flagged() {
        let mut s = clean_stats();
        s.lifecycle_ops.push(lop(
            "life-1",
            120,
            LifecycleOp::Edit,
            LifecycleResult::Committed,
            Some("1:r"),
        ));
        s.lifecycle_ops.push(lop(
            "life-1",
            121,
            LifecycleOp::Delete,
            LifecycleResult::Committed,
            Some("1:r"),
        ));
        assert_eq!(check(&s), vec![]);
    }

    /// Two committed Edits share one base: a lost update, with no delete
    /// involved at all.
    #[test]
    fn two_edits_on_one_base_are_flagged() {
        let mut s = clean_stats();
        s.lifecycle_ops.push(lop(
            "life-1",
            101,
            LifecycleOp::Edit,
            LifecycleResult::Committed,
            Some("9:zzz"),
        ));
        s.lifecycle_ops.push(lop(
            "life-1",
            102,
            LifecycleOp::Edit,
            LifecycleResult::Committed,
            Some("9:zzz"),
        ));
        assert_eq!(
            check(&s),
            vec![Violation::StaleBaseCommitted {
                key: "life-1".into(),
                base: "9:zzz".into(),
            }]
        );
    }

    /// Two committed Deletes sharing a base are legitimate — the second is a
    /// no-op delete over the tombstone the first just wrote, and still reports
    /// `Deleted`. Its evidence read finds the first delete's tombstone, whose
    /// parent is the shared base, so both can carry `Some(true)`. Must not be
    /// flagged.
    #[test]
    fn concurrent_noop_deletes_share_a_base_without_violation() {
        let mut s = clean_stats();
        s.lifecycle_ops.push(wrote(
            lop(
                "life-0",
                103,
                LifecycleOp::Delete,
                LifecycleResult::Committed,
                Some("0:aaa"), // same base as the existing committed Delete op-id 1
            ),
            Some(true),
        ));
        assert_eq!(check(&s), vec![]);
    }

    /// A committed Delete and an Edit that lost the race (`Conflict`) on
    /// the same base is the expected, correct outcome of OCC — only one
    /// conditional write may commit. Must not be flagged.
    #[test]
    fn conflicted_edit_on_a_deleted_base_is_fine() {
        let mut s = clean_stats();
        s.lifecycle_ops.push(lop(
            "life-0",
            104,
            LifecycleOp::Edit,
            LifecycleResult::Conflict,
            Some("0:aaa"), // same base as the existing committed Delete op-id 1
        ));
        assert_eq!(check(&s), vec![]);
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
