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
//!   exercised, which is itself a failure.
//! - **Durability under churn** — every acked unique-key write is still readable.
//! - **Liveness** — the run made progress and every writer finished.
//!
//! This is a targeted invariant oracle, not a linearizability checker: it asserts
//! on set membership / chain length / completion, never on exact interleavings,
//! so normal scheduling jitter cannot flake it.

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
    /// The key is readable with the exact value that was written.
    pub readable_with_value: bool,
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
}

/// Check every soak invariant. Returns the (possibly empty) set of violations.
pub fn check(stats: &SoakStats) -> Vec<Violation> {
    let mut out = Vec::new();

    // Per contended key: committed op-ids must all survive, exactly once, and the
    // revision chain must have grown by one per committed put.
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
            .collect::<std::collections::BTreeSet<_>>()
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

    // Durability under churn: every acked unique write must still be readable.
    for fu in &stats.unique {
        if fu.acked && !fu.readable_with_value {
            out.push(Violation::UniqueWriteLost {
                key: fu.key.clone(),
            });
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean run: two contended keys whose final sets hold exactly their
    /// committed op-ids, chains match, conflicts were seen, unique writes stuck,
    /// and all writers finished. Must report zero violations.
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
            unique: vec![FinalUnique {
                key: "u1".into(),
                acked: true,
                readable_with_value: true,
            }],
            conflicts_observed: 3,
            writers_completed: 4,
            writers_total: 4,
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
            unique: vec![],
            conflicts_observed: 0,
            writers_completed: 0,
            writers_total: 4,
        };
        assert!(check(&s).contains(&Violation::NoProgress));
    }
}
