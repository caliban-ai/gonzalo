//! Reconcile two `Store`s. Any store can be a sync peer. Append-only kinds
//! auto-merge by union; structured/opaque divergences are surfaced as
//! conflicts. No stored ancestry yet (M2): the merge uses an empty base,
//! which is correct for append-only union.

use crate::{
    Body, Identity, KeyPrefix, MergeOutcome, Meta, Record, RecordKey, Result, Revision, Store,
    merge,
};
use std::collections::BTreeSet;

/// A divergence that could not be auto-merged and needs caller/CLI resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncConflict {
    pub key: RecordKey,
    pub a: Box<Record>,
    pub b: Box<Record>,
}

/// What a sync run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use = "a SyncReport may contain unresolved conflicts that must be handled"]
pub struct SyncReport {
    /// Keys copied into store A (were only in B).
    pub copied_to_a: Vec<RecordKey>,
    /// Keys copied into store B (were only in A).
    pub copied_to_b: Vec<RecordKey>,
    /// Keys auto-merged (append-only) and written to both stores.
    pub merged: Vec<RecordKey>,
    /// Divergences needing manual resolution.
    pub conflicts: Vec<SyncConflict>,
}

/// Reconcile stores `a` and `b`. After a clean run (no `conflicts`), both
/// stores hold the same set of records for every key.
pub async fn sync(a: &dyn Store, b: &dyn Store) -> Result<SyncReport> {
    let mut report = SyncReport::default();

    let mut keys: BTreeSet<RecordKey> = BTreeSet::new();
    keys.extend(a.list(&KeyPrefix::default()).await?);
    keys.extend(b.list(&KeyPrefix::default()).await?);

    for key in keys {
        let ra = a.get(&key).await?;
        let rb = b.get(&key).await?;
        match (ra, rb) {
            (Some(rec), None) => {
                copy(b, &rec).await?;
                report.copied_to_b.push(key);
            }
            (None, Some(rec)) => {
                copy(a, &rec).await?;
                report.copied_to_a.push(key);
            }
            (Some(rec_a), Some(rec_b)) => {
                if rec_a.revision == rec_b.revision {
                    continue; // already in sync
                }
                match merge(
                    rec_a.kind.merge_class(),
                    &Body::Inline(Vec::new()),
                    &rec_a.body,
                    &rec_b.body,
                ) {
                    MergeOutcome::Merged(body) => {
                        let merged = build_merged(&key, &rec_a, &rec_b, body);
                        overwrite(a, &merged, &rec_a.revision).await?;
                        overwrite(b, &merged, &rec_b.revision).await?;
                        report.merged.push(key);
                    }
                    MergeOutcome::NeedsResolution => {
                        report.conflicts.push(SyncConflict {
                            key,
                            a: Box::new(rec_a),
                            b: Box::new(rec_b),
                        });
                    }
                }
            }
            (None, None) => {}
        }
    }
    Ok(report)
}

async fn copy(dst: &dyn Store, rec: &Record) -> Result<()> {
    // Create in dst; if dst already changed concurrently we leave it (M2).
    let _ = dst.put(rec.clone(), None).await?;
    Ok(())
}

async fn overwrite(dst: &dyn Store, rec: &Record, expected: &Revision) -> Result<()> {
    // Conflict means a concurrent mutation raced the merge window. M2 assumes
    // quiescent stores; the report won't reflect this case. Tightening to a
    // re-loop is deferred.
    let _ = dst.put(rec.clone(), Some(expected.clone())).await?;
    Ok(())
}

fn build_merged(key: &RecordKey, a: &Record, b: &Record, body: Body) -> Record {
    let counter = a.revision.counter.max(b.revision.counter) + 1;
    let mut labels = a.meta.labels.clone();
    labels.extend(b.meta.labels.clone());
    let mut links = a.links.clone();
    for l in &b.links {
        if !links.contains(l) {
            links.push(l.clone());
        }
    }
    Record {
        key: key.clone(),
        kind: a.kind,
        revision: Revision {
            counter,
            hash: crate::ContentHash::of(body.bytes()),
        },
        parent: Some(if a.revision.counter >= b.revision.counter {
            a.revision.clone()
        } else {
            b.revision.clone()
        }),
        body,
        meta: Meta {
            author: Identity::new("gonzalo-sync"),
            origin_system: "sync".into(),
            created: a.meta.created.min(b.meta.created),
            updated: a.meta.updated.max(b.meta.updated),
            labels,
        },
        links,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PutResult, RecordKind, store::Conflict};
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemStore(Mutex<BTreeMap<RecordKey, Record>>);

    #[async_trait]
    impl Store for MemStore {
        async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            let mut g = self.0.lock().unwrap();
            let current = g.get(&record.key).map(|r| r.revision.clone());
            if current != expected {
                if let Some(cur) = g.get(&record.key).cloned() {
                    return Ok(PutResult::Conflict(Box::new(Conflict {
                        key: record.key.clone(),
                        expected,
                        current: cur,
                    })));
                }
                return Err(crate::CoreError::NotFound(record.key.clone()));
            }
            let rev = record.revision.clone();
            g.insert(record.key.clone(), record);
            Ok(PutResult::Committed(rev))
        }
        async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .keys()
                .filter(|k| prefix.matches(k))
                .cloned()
                .collect())
        }
    }

    fn rec(id: &str, kind: RecordKind, payload: &str) -> Record {
        let body = Body::Inline(payload.as_bytes().to_vec());
        Record {
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            kind,
            key: RecordKey::new("ns", "col", id),
            meta: Meta {
                author: Identity::new("t"),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
        }
    }

    #[tokio::test]
    async fn copies_one_sided_records_both_directions() {
        let a = MemStore::default();
        let b = MemStore::default();
        let _ = a
            .put(rec("only_a", RecordKind::Topic, "x"), None)
            .await
            .unwrap();
        let _ = b
            .put(rec("only_b", RecordKind::Topic, "y"), None)
            .await
            .unwrap();

        let report = sync(&a, &b).await.unwrap();
        assert_eq!(
            report.copied_to_b,
            vec![RecordKey::new("ns", "col", "only_a")]
        );
        assert_eq!(
            report.copied_to_a,
            vec![RecordKey::new("ns", "col", "only_b")]
        );
        assert!(
            a.get(&RecordKey::new("ns", "col", "only_b"))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            b.get(&RecordKey::new("ns", "col", "only_a"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn append_only_divergence_auto_merges() {
        let a = MemStore::default();
        let b = MemStore::default();
        let _ = a
            .put(rec("t", RecordKind::Topic, "base\nfrom_a\n"), None)
            .await
            .unwrap();
        let _ = b
            .put(rec("t", RecordKind::Topic, "base\nfrom_b\n"), None)
            .await
            .unwrap();

        let report = sync(&a, &b).await.unwrap();
        assert_eq!(report.merged, vec![RecordKey::new("ns", "col", "t")]);
        assert!(report.conflicts.is_empty());
        let merged = a
            .get(&RecordKey::new("ns", "col", "t"))
            .await
            .unwrap()
            .unwrap();
        let text = String::from_utf8(merged.body.bytes().to_vec()).unwrap();
        assert!(text.contains("from_a") && text.contains("from_b") && text.contains("base"));
        // Both stores converge to the same revision.
        let mb = b
            .get(&RecordKey::new("ns", "col", "t"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(merged.revision, mb.revision);
    }

    #[tokio::test]
    async fn checkpoint_divergence_surfaces_conflict() {
        let a = MemStore::default();
        let b = MemStore::default();
        let _ = a
            .put(rec("c", RecordKind::Checkpoint, "a"), None)
            .await
            .unwrap();
        let _ = b
            .put(rec("c", RecordKind::Checkpoint, "b"), None)
            .await
            .unwrap();

        let report = sync(&a, &b).await.unwrap();
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.conflicts[0].key, RecordKey::new("ns", "col", "c"));
        assert!(report.merged.is_empty());
    }

    #[tokio::test]
    async fn memory_tier_divergence_surfaces_conflict() {
        let a = MemStore::default();
        let b = MemStore::default();
        let _ = a
            .put(rec("m", RecordKind::MemoryTier, "a"), None)
            .await
            .unwrap();
        let _ = b
            .put(rec("m", RecordKind::MemoryTier, "b"), None)
            .await
            .unwrap();

        let report = sync(&a, &b).await.unwrap();
        assert_eq!(report.conflicts.len(), 1);
        assert!(report.merged.is_empty());
    }

    #[tokio::test]
    async fn session_divergence_auto_merges() {
        let a = MemStore::default();
        let b = MemStore::default();
        let _ = a
            .put(rec("s", RecordKind::Session, "base\nfrom_a\n"), None)
            .await
            .unwrap();
        let _ = b
            .put(rec("s", RecordKind::Session, "base\nfrom_b\n"), None)
            .await
            .unwrap();

        let report = sync(&a, &b).await.unwrap();
        assert_eq!(report.merged, vec![RecordKey::new("ns", "col", "s")]);
        assert!(report.conflicts.is_empty());
    }
}
