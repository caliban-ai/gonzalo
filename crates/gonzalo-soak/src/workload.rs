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
    format!("life-{}", ((life_id / 4) as usize) % lifecycle_keys.max(1))
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
        handles.push(tokio::spawn(
            async move { writer(w, d, c, ids, lids).await },
        ));
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

/// Retry a final read up to three attempts, 200 ms apart, on errors only, so a
/// connection pooled to a restarted replica can't fail the soak. A read that
/// keeps failing still returns its last error (a `FinalReadFailed` violation).
async fn read_settled<T>(
    mut read: impl AsyncFnMut() -> gonzalo_core::Result<T>,
) -> gonzalo_core::Result<T> {
    let mut last = read().await;
    for _ in 0..2 {
        if last.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        last = read().await;
    }
    last
}

/// Read every lifecycle key from **every** replica directly (raw and consumer).
/// Call only after the writers have stopped and all replicas are live: a final
/// read retries up to 3 attempts, 200 ms apart, on errors only, so a connection
/// pooled to a just-restarted replica doesn't fail the soak on its own; a
/// replica that stays unreadable is still reported (`FinalReadFailed`).
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
            let raw = raw_state(read_settled(async || store.get_raw(&key).await).await);
            let consumer_live = read_settled(async || store.get(&key).await)
                .await
                .ok()
                .map(|r| r.is_some());
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
        (LifecycleOp::Delete, None)
        | (LifecycleOp::Edit, None)
        | (LifecycleOp::Recreate, Some(_)) => LifecycleResult::Skipped,
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
        let raw_tombstone =
            was_deleted && matches!(d.get_raw(&key).await, Ok(Some(rec)) if rec.is_tombstone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::{LifecycleOp, LifecycleResult, RawState};
    use gonzalo_core::{DEFAULT_ANCESTOR_CAP, Store, tombstone_of};
    use gonzalo_store_fs::FsStore;
    use std::collections::BTreeSet;
    use std::sync::atomic::AtomicUsize;

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

    /// `read_settled` retries only on error, up to three attempts 200 ms apart,
    /// and returns the last outcome either way (R1).
    #[tokio::test]
    async fn read_settled_retries_errors_then_returns_the_last() {
        // Errors twice, then Ok: returns Ok after exactly 3 calls.
        let calls = AtomicUsize::new(0);
        let out = read_settled(|| {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if n < 3 {
                    Err(gonzalo_core::CoreError::Backend("down".into()))
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // Always errors: returns Err after exactly 3 calls.
        let calls = AtomicUsize::new(0);
        let out: gonzalo_core::Result<()> = read_settled(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move { Err(gonzalo_core::CoreError::Backend("down".into())) }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // Immediate Ok: exactly 1 call.
        let calls = AtomicUsize::new(0);
        let out = read_settled(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok(42) }
        })
        .await;
        assert_eq!(out.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
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
            lifecycle_ops_per_writer: 80,
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
