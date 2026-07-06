//! Reconcile two `Store`s. Any store can be a sync peer. Append-only kinds
//! auto-merge by union; structured/opaque divergences are surfaced as
//! conflicts. [`sync_with_ancestry`] 3-way-merges structured bodies against
//! their real common ancestor when an [`AncestryStore`](crate::AncestryStore)
//! retains it; [`sync`] uses an empty base (correct for append-only union, safe
//! otherwise) — see ADR 0016.

use crate::{
    BlobStore, Body, Identity, KeyPrefix, MergeOutcome, Meta, PutResult, Record, RecordKey, Result,
    Revision, Store, merge,
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

/// Upper bound on sync passes before giving up on a non-quiescent pair.
///
/// Each pass re-reads both stores, so a store that settles converges within
/// one extra pass. The cap only bites when writers never stop racing the merge
/// window (livelock guard): rather than spin forever, sync returns the last
/// pass's best-effort report.
const MAX_SYNC_PASSES: usize = 16;

/// Reconcile stores `a` and `b`. After a clean run (no `conflicts`), both
/// stores hold the same set of records for every key.
///
/// Stores need not be quiescent. A single pass can lose a write that lands in
/// the read→merge→write window (the OCC `put` returns `Conflict`); sync re-runs
/// the pass until one completes without any such race (a fixpoint), bounded by
/// [`MAX_SYNC_PASSES`] so continuous concurrent writes can't livelock it.
pub async fn sync(a: &dyn Store, b: &dyn Store) -> Result<SyncReport> {
    sync_with_ancestry(a, b, None).await
}

/// As [`sync`], but 3-way-merges divergent `Structured` bodies against their
/// real common ancestor when one is available. `ancestry` is a content-addressed
/// store of past bodies keyed by revision hash (see
/// [`AncestryStore`](crate::AncestryStore)): when two records diverge from a
/// shared parent revision whose body it holds, that body is the merge base;
/// otherwise sync falls back to the empty base (ADR 0016).
pub async fn sync_with_ancestry(
    a: &dyn Store,
    b: &dyn Store,
    ancestry: Option<&dyn BlobStore>,
) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    for _ in 0..MAX_SYNC_PASSES {
        let (pass, raced) = sync_pass(a, b, ancestry).await?;
        report = pass;
        if !raced {
            break; // quiescent: this pass landed cleanly, stores have converged.
        }
    }
    Ok(report)
}

/// The merge base for a divergence: the body of `a`/`b`'s shared parent revision
/// when `ancestry` retains it, else an empty base (the base-agnostic fallback,
/// correct for `AppendOnly` and safe for the rest).
async fn ancestry_base(ancestry: Option<&dyn BlobStore>, rec_a: &Record, rec_b: &Record) -> Body {
    if let Some(anc) = ancestry
        && let (Some(pa), Some(pb)) = (&rec_a.parent, &rec_b.parent)
        && pa == pb
        && let Ok(Some(bytes)) = anc.get_blob(&pa.hash).await
    {
        return Body::Inline(bytes);
    }
    Body::Inline(Vec::new())
}

/// One reconciliation pass over the union of keys. Returns the pass's report
/// and whether any write lost an OCC race (`true` ⇒ a store changed mid-pass,
/// so the caller should re-loop). A `NeedsResolution` merge conflict is a
/// terminal divergence (surfaced in the report), not a race, and does not
/// trigger a re-loop.
async fn sync_pass(
    a: &dyn Store,
    b: &dyn Store,
    ancestry: Option<&dyn BlobStore>,
) -> Result<(SyncReport, bool)> {
    let mut report = SyncReport::default();
    let mut raced = false;

    let mut keys: BTreeSet<RecordKey> = BTreeSet::new();
    keys.extend(a.list(&KeyPrefix::default()).await?);
    keys.extend(b.list(&KeyPrefix::default()).await?);

    for key in keys {
        let ra = a.get(&key).await?;
        let rb = b.get(&key).await?;
        match (ra, rb) {
            (Some(rec), None) => {
                if copy(b, &rec).await? {
                    report.copied_to_b.push(key);
                } else {
                    raced = true;
                }
            }
            (None, Some(rec)) => {
                if copy(a, &rec).await? {
                    report.copied_to_a.push(key);
                } else {
                    raced = true;
                }
            }
            (Some(rec_a), Some(rec_b)) => {
                if rec_a.revision == rec_b.revision {
                    continue; // already in sync
                }
                let base = ancestry_base(ancestry, &rec_a, &rec_b).await;
                match merge(rec_a.kind.merge_class(), &base, &rec_a.body, &rec_b.body) {
                    MergeOutcome::Merged(body) => {
                        let merged = build_merged(&key, &rec_a, &rec_b, body);
                        let la = overwrite(a, &merged, &rec_a.revision).await?;
                        let lb = overwrite(b, &merged, &rec_b.revision).await?;
                        if la && lb {
                            report.merged.push(key);
                        } else {
                            // At least one side raced; re-loop to reconcile the
                            // store that moved against the now-merged peer.
                            raced = true;
                        }
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
    Ok((report, raced))
}

/// Create `rec` in `dst`. Returns `false` if `dst` changed concurrently
/// (the key already exists), signalling the caller to re-loop.
async fn copy(dst: &dyn Store, rec: &Record) -> Result<bool> {
    Ok(matches!(
        dst.put(rec.clone(), None).await?,
        PutResult::Committed(_)
    ))
}

/// Conditionally overwrite `dst` with `rec`, expecting revision `expected`.
/// Returns `false` if a concurrent mutation raced the merge window (`put`
/// returned `Conflict`), signalling the caller to re-loop.
async fn overwrite(dst: &dyn Store, rec: &Record, expected: &Revision) -> Result<bool> {
    Ok(matches!(
        dst.put(rec.clone(), Some(expected.clone())).await?,
        PutResult::Committed(_)
    ))
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

    /// A store that returns one spurious `Conflict` on the first conditional
    /// (`expected.is_some()`) `put` per key — a concurrent writer that races
    /// the first overwrite — then behaves normally. Forces the sync re-loop to
    /// retry and still converge.
    #[derive(Default)]
    struct FlakyOnceStore {
        inner: Mutex<BTreeMap<RecordKey, Record>>,
        tripped: Mutex<std::collections::HashSet<RecordKey>>,
    }

    #[async_trait]
    impl Store for FlakyOnceStore {
        async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
            Ok(self.inner.lock().unwrap().get(key).cloned())
        }
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            if expected.is_some() && self.tripped.lock().unwrap().insert(record.key.clone()) {
                let current = self
                    .inner
                    .lock()
                    .unwrap()
                    .get(&record.key)
                    .cloned()
                    .unwrap();
                return Ok(PutResult::Conflict(Box::new(Conflict {
                    key: record.key.clone(),
                    expected,
                    current,
                })));
            }
            let mut g = self.inner.lock().unwrap();
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
                .inner
                .lock()
                .unwrap()
                .keys()
                .filter(|k| prefix.matches(k))
                .cloned()
                .collect())
        }
    }

    /// A store whose conditional `put` *always* races (a concurrent writer that
    /// never stops). Initial creates (`expected == None`) commit; every
    /// overwrite conflicts. Used to prove the re-loop is bounded and terminates.
    #[derive(Default)]
    struct AlwaysRacyStore(Mutex<BTreeMap<RecordKey, Record>>);

    #[async_trait]
    impl Store for AlwaysRacyStore {
        async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            let mut g = self.0.lock().unwrap();
            match expected {
                None if !g.contains_key(&record.key) => {
                    let rev = record.revision.clone();
                    g.insert(record.key.clone(), record);
                    Ok(PutResult::Committed(rev))
                }
                _ => {
                    let current = g.get(&record.key).cloned().unwrap();
                    Ok(PutResult::Conflict(Box::new(Conflict {
                        key: record.key.clone(),
                        expected,
                        current,
                    })))
                }
            }
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

    #[tokio::test]
    async fn re_loops_until_a_racing_store_converges() {
        // B races the first overwrite (non-quiescent during the merge window).
        // A single pass would swallow that conflict and leave B un-synced; the
        // re-loop must retry until both stores converge.
        let a = MemStore::default();
        let b = FlakyOnceStore::default();
        let _ = a
            .put(rec("t", RecordKind::Topic, "base\nfrom_a\n"), None)
            .await
            .unwrap();
        let _ = b
            .put(rec("t", RecordKind::Topic, "base\nfrom_b\n"), None)
            .await
            .unwrap();

        let report = sync(&a, &b).await.unwrap();

        assert!(report.conflicts.is_empty());
        let key = RecordKey::new("ns", "col", "t");
        let ra = a.get(&key).await.unwrap().unwrap();
        let rb = b.get(&key).await.unwrap().unwrap();
        // Both stores converged despite B racing the first overwrite.
        assert_eq!(ra.revision, rb.revision);
        let text = String::from_utf8(rb.body.bytes().to_vec()).unwrap();
        assert!(text.contains("from_a") && text.contains("from_b"));
        assert_eq!(report.merged, vec![key]);
    }

    /// Build ours/theirs Structured records diverging from a shared base
    /// revision (disjoint field edits), plus the base body to retain.
    fn structured_divergence() -> (Record, Record, &'static str) {
        let base = rec("m", RecordKind::MemoryTier, r#"{"name":"a","content":"x"}"#);
        let base_rev = base.revision.clone();
        let mut ours = rec("m", RecordKind::MemoryTier, r#"{"name":"b","content":"x"}"#);
        ours.parent = Some(base_rev.clone());
        let mut theirs = rec("m", RecordKind::MemoryTier, r#"{"name":"a","content":"y"}"#);
        theirs.parent = Some(base_rev);
        (ours, theirs, r#"{"name":"a","content":"x"}"#)
    }

    #[tokio::test]
    async fn structured_divergence_merges_with_ancestry() {
        use crate::ancestry::tests::Mem;
        let a = Mem::default();
        let b = Mem::default();
        let ancestry = Mem::default();
        let (ours, theirs, base_body) = structured_divergence();
        // Retain the shared base body under its revision hash.
        ancestry.put_blob(base_body.as_bytes()).await.unwrap();
        let _ = a.put(ours, None).await.unwrap();
        let _ = b.put(theirs, None).await.unwrap();

        let report = sync_with_ancestry(&a, &b, Some(&ancestry)).await.unwrap();

        let key = RecordKey::new("ns", "col", "m");
        assert_eq!(report.merged, vec![key.clone()], "3-way merged");
        assert!(report.conflicts.is_empty());
        // Disjoint field edits both applied against the real base.
        let merged = a.get(&key).await.unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_slice(merged.body.bytes()).unwrap();
        assert_eq!(v, serde_json::json!({"name": "b", "content": "y"}));
    }

    #[tokio::test]
    async fn structured_divergence_conflicts_without_ancestry() {
        use crate::ancestry::tests::Mem;
        let a = Mem::default();
        let b = Mem::default();
        let (ours, theirs, _) = structured_divergence();
        let _ = a.put(ours, None).await.unwrap();
        let _ = b.put(theirs, None).await.unwrap();

        // No ancestry → empty base → the Structured merge cannot tell a one-sided
        // edit from a real conflict, so it surfaces a conflict.
        let report = sync(&a, &b).await.unwrap();
        assert_eq!(report.conflicts.len(), 1);
        assert!(report.merged.is_empty());
    }

    #[tokio::test]
    async fn bounded_retry_terminates_under_continuous_writes() {
        // Both stores race *every* overwrite — a non-quiescent pair that never
        // settles. The re-loop must be bounded: sync returns (does not hang)
        // and reports the divergence as unresolved rather than spinning.
        let a = AlwaysRacyStore::default();
        let b = AlwaysRacyStore::default();
        let _ = a
            .put(rec("t", RecordKind::Topic, "base\nfrom_a\n"), None)
            .await
            .unwrap();
        let _ = b
            .put(rec("t", RecordKind::Topic, "base\nfrom_b\n"), None)
            .await
            .unwrap();

        let report = sync(&a, &b).await.unwrap();

        // No overwrite ever committed, so nothing converged.
        let key = RecordKey::new("ns", "col", "t");
        let ra = a.get(&key).await.unwrap().unwrap();
        let rb = b.get(&key).await.unwrap().unwrap();
        assert_ne!(ra.revision, rb.revision);
        assert!(report.merged.is_empty());
    }
}
