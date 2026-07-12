//! The concurrent write workload and result collection for the [`oracle`].
//!
//! Each writer runs two op streams against the replica [`Dispatcher`]:
//!
//! - **Contended RMW** on a small set of shared keys — read the current record,
//!   append this op's globally-unique id to the record's comma-separated set,
//!   and conditionally `put` against the revision just read. On `Conflict`,
//!   re-read and retry (bounded). This is the arbitration proof: if conditional
//!   writes are correct, every *committed* op-id survives in the final set.
//! - **Unique-key writes** — disjoint keys written once, later read back to prove
//!   no acked write is lost across replica kills.
//!
//! After all writers finish, [`run`] reads every key's final state and returns a
//! [`SoakStats`] for [`oracle::check`].
//!
//! [`oracle`]: crate::oracle
//! [`oracle::check`]: crate::oracle::check

use crate::dispatch::Dispatcher;
use crate::oracle::{FinalContended, FinalUnique, OpRecord, OpResult, SoakStats};
use gonzalo_core::{Body, Identity, Meta, PutResult, Record, RecordKey, RecordKind, Revision};
use std::collections::BTreeMap;
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
        }
    }
}

/// Run the workload to completion and collect stats for the oracle. `dispatcher`
/// fans ops across the live replicas; chaos (replica kills) is driven separately
/// by the caller while this runs.
pub async fn run(dispatcher: Arc<Dispatcher>, cfg: WorkloadConfig) -> SoakStats {
    let op_ids = Arc::new(AtomicU64::new(1));
    let mut handles = Vec::new();
    for w in 0..cfg.writers {
        let d = dispatcher.clone();
        let c = cfg.clone();
        let ids = op_ids.clone();
        handles.push(tokio::spawn(async move { writer(w, d, c, ids).await }));
    }

    let mut ops = Vec::new();
    let mut unique_acked: Vec<(String, Vec<u8>)> = Vec::new();
    let mut conflicts_observed = 0u64;
    let mut writers_completed = 0;
    for h in handles {
        if let Ok(res) = h.await {
            ops.extend(res.ops);
            unique_acked.extend(res.unique_acked);
            conflicts_observed += res.conflicts;
            writers_completed += 1;
        }
    }

    let contended = collect_contended(&dispatcher, &cfg).await;
    let unique = collect_unique(&dispatcher, &cfg, &unique_acked).await;

    SoakStats {
        ops,
        contended,
        unique,
        conflicts_observed,
        writers_completed,
        writers_total: cfg.writers,
    }
}

struct WriterResult {
    ops: Vec<OpRecord>,
    unique_acked: Vec<(String, Vec<u8>)>,
    conflicts: u64,
}

async fn writer(
    w: usize,
    d: Arc<Dispatcher>,
    cfg: WorkloadConfig,
    op_ids: Arc<AtomicU64>,
) -> WriterResult {
    let mut ops = Vec::with_capacity(cfg.ops_per_writer);
    let mut conflicts = 0u64;
    for _ in 0..cfg.ops_per_writer {
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

    let mut unique_acked = Vec::with_capacity(cfg.unique_per_writer);
    for i in 0..cfg.unique_per_writer {
        let key_id = format!("unique-{w}-{i}");
        let value = key_id.clone().into_bytes();
        if create(&d, &cfg, &key_id, &value).await {
            unique_acked.push((key_id, value));
        }
    }

    WriterResult {
        ops,
        unique_acked,
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

/// Create a unique key once. Returns `true` if the write was acked (`Committed`).
async fn create(d: &Dispatcher, cfg: &WorkloadConfig, key_id: &str, value: &[u8]) -> bool {
    let key = RecordKey::new(&cfg.namespace, &cfg.collection, key_id);
    let record = build_record(&key, value, None);
    matches!(d.put(record, None).await, Ok(PutResult::Committed(_)))
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
) -> Vec<FinalUnique> {
    let mut out = Vec::new();
    for (key_id, value) in acked {
        let key = RecordKey::new(&cfg.namespace, &cfg.collection, key_id);
        let readable_with_value = matches!(
            d.get(&key).await,
            Ok(Some(rec)) if rec.body.bytes() == value.as_slice()
        );
        out.push(FinalUnique {
            key: key_id.clone(),
            acked: true,
            readable_with_value,
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
    use gonzalo_core::Store;
    use gonzalo_store_fs::FsStore;

    #[test]
    fn parse_round_trips() {
        assert_eq!(parse_members(&encode_members(&[3, 1, 2])), vec![3, 1, 2]);
        assert_eq!(parse_members(b""), Vec::<u64>::new());
    }

    /// End-to-end workload against three in-process `FsStore` replicas over one
    /// shared directory — real concurrency + real conditional-write arbitration,
    /// no external S3 backend. Proves the RMW/oracle/dispatch integration holds the invariant.
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
            ..Default::default()
        };
        let stats = run(dispatcher, cfg).await;

        let violations = crate::oracle::check(&stats);
        assert!(violations.is_empty(), "invariant violated: {violations:?}");
    }
}
