//! Reconcile two `Store`s. Any store can be a sync peer.
//!
//! Sync reads through the replication surface (`list_raw` / `get_raw`), so it
//! sees tombstones. For a key held by both sides it uses each record's bounded
//! `ancestors` list (spec §3.4) to tell "one side is behind" (overwrite it)
//! from "the sides diverged". Diverged tombstones converge on the higher
//! revision. A tombstone against a live edit is a conflict. Two diverged live
//! records take the class-aware merge: append-only kinds union, and
//! structured/opaque divergences surface as conflicts.
//! [`sync_with_ancestry`] 3-way-merges structured bodies against their real
//! common ancestor when an [`AncestryStore`](crate::AncestryStore) retains it;
//! [`sync`] uses an empty base (ADR 0016).

use crate::tombstone::tombstone_winner;
use crate::{
    BlobStore, Body, CoreError, Identity, KeyPrefix, MergeOutcome, PutResult, Record, RecordKey,
    Result, Revision, Store, merge,
};
use std::collections::BTreeSet;

/// A divergence that could not be auto-merged and needs caller/CLI resolution:
/// an unmergeable body divergence, or a delete (one side a tombstone) against a
/// concurrent edit. Neither side is written.
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
    /// Keys copied into store A (were only in B). Includes tombstones.
    pub copied_to_a: Vec<RecordKey>,
    /// Keys copied into store B (were only in A). Includes tombstones.
    pub copied_to_b: Vec<RecordKey>,
    /// Keys where A was behind B's revision chain and was overwritten with B's
    /// record. Includes tombstones.
    pub fast_forwarded_to_a: Vec<RecordKey>,
    /// Keys where B was behind A's revision chain and was overwritten with A's
    /// record. Includes tombstones.
    pub fast_forwarded_to_b: Vec<RecordKey>,
    /// Keys whose divergence was reconciled and written to both stores: an
    /// auto-merged live record, or two diverged tombstones converged on the
    /// winner.
    pub merged: Vec<RecordKey>,
    /// Divergences needing manual resolution.
    pub conflicts: Vec<SyncConflict>,
    /// Keys whose write still lost a race on the final pass, when sync gave up
    /// after [`MAX_SYNC_PASSES`]. Empty on every run that reached a clean pass,
    /// so a non-empty list means **the stores may still disagree on these keys,
    /// even when `conflicts` is empty** (gonzalo#290). Re-run sync once the
    /// writers settle.
    pub unconverged: Vec<RecordKey>,
}

impl SyncReport {
    /// Whether sync reached a pass in which no write lost a race. `false` means
    /// the pass limit was exhausted and the stores may still disagree on
    /// [`unconverged`](Self::unconverged); it says nothing about `conflicts`,
    /// which are a settled outcome a caller resolves.
    #[must_use]
    pub fn converged(&self) -> bool {
        self.unconverged.is_empty()
    }
}

/// Upper bound on sync passes before giving up on a non-quiescent pair.
///
/// Each pass re-reads both stores, so a store that settles converges within
/// one extra pass. The cap only bites when writers never stop racing the merge
/// window (livelock guard): rather than spin forever, sync returns the last
/// pass's best-effort report.
const MAX_SYNC_PASSES: usize = 16;

/// Reconcile stores `a` and `b`. After a run with no `conflicts` that did not
/// exhaust [`MAX_SYNC_PASSES`], both stores hold the same record (live or
/// tombstone) for every key.
///
/// Check [`SyncReport::converged`] as well as `conflicts`: an exhausted run
/// reports the still-racing keys in [`SyncReport::unconverged`], and treating
/// its empty `conflicts` list as "in sync" would be wrong (gonzalo#290).
///
/// Stores need not be quiescent. A single pass can lose a write that lands in
/// the read→merge→write window (the OCC `put_raw` returns `Conflict`, or
/// `NotFound` when the key was purged meanwhile); sync re-runs the pass until one
/// completes without any such race (a fixpoint), bounded by
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
        if raced.is_empty() {
            break; // quiescent: this pass landed cleanly, stores have converged.
        }
        // Carried on the report only if this turns out to be the last pass:
        // a later clean pass replaces the whole report, leaving it empty.
        report.unconverged = raced;
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

/// How two present records for the same key relate by revision chain (§3.4).
#[derive(Debug, PartialEq, Eq)]
enum Relation {
    /// Equal revisions.
    InSync,
    /// B's revision is in A's ancestors: B is behind.
    AAhead,
    /// A's revision is in B's ancestors: A is behind.
    BAhead,
    /// Neither chain contains the other's revision: diverged, or the chain is
    /// unknown (legacy empty list, or truncated past the cap). Fails safe.
    Diverged,
}

fn relate(a: &Record, b: &Record) -> Relation {
    if a.revision == b.revision {
        Relation::InSync
    } else if a.ancestors.contains(&b.revision) {
        Relation::AAhead
    } else if b.ancestors.contains(&a.revision) {
        Relation::BAhead
    } else {
        Relation::Diverged
    }
}

/// The cap sync folds with: large enough never to truncate. Each input list is
/// already bounded by its own store's cap, and each destination store's
/// `plan_put_raw` truncates to that store's cap on write, so the store's cap
/// wins.
fn lossless_cap(a: &Record, b: &Record) -> usize {
    a.ancestors.len() + b.ancestors.len() + 2
}

/// One reconciliation pass over the union of raw keys. Returns the pass's
/// report and the keys whose write lost a race (non-empty ⇒ a store changed
/// mid-pass, so the caller should re-loop). A `SyncConflict` is a terminal
/// divergence (surfaced in the report), not a race, and does not trigger a
/// re-loop.
async fn sync_pass(
    a: &dyn Store,
    b: &dyn Store,
    ancestry: Option<&dyn BlobStore>,
) -> Result<(SyncReport, Vec<RecordKey>)> {
    let mut report = SyncReport::default();
    let mut raced: Vec<RecordKey> = Vec::new();

    // Raw reads only: consumer reads hide tombstones, and a tombstone that sync
    // cannot see is copied over by the peer's live record (resurrection).
    let mut keys: BTreeSet<RecordKey> = BTreeSet::new();
    keys.extend(a.list_raw(&KeyPrefix::default()).await?);
    keys.extend(b.list_raw(&KeyPrefix::default()).await?);

    for key in keys {
        let ra = a.get_raw(&key).await?;
        let rb = b.get_raw(&key).await?;
        match (ra, rb) {
            (Some(rec), None) => {
                if copy(b, &rec).await? {
                    report.copied_to_b.push(key);
                } else {
                    raced.push(key);
                }
            }
            (None, Some(rec)) => {
                if copy(a, &rec).await? {
                    report.copied_to_a.push(key);
                } else {
                    raced.push(key);
                }
            }
            (Some(rec_a), Some(rec_b)) => match relate(&rec_a, &rec_b) {
                Relation::InSync => {}
                Relation::AAhead => {
                    if overwrite(b, &rec_a, &rec_b.revision).await? {
                        report.fast_forwarded_to_b.push(key);
                    } else {
                        raced.push(key);
                    }
                }
                Relation::BAhead => {
                    if overwrite(a, &rec_b, &rec_a.revision).await? {
                        report.fast_forwarded_to_a.push(key);
                    } else {
                        raced.push(key);
                    }
                }
                Relation::Diverged => match (rec_a.is_tombstone(), rec_b.is_tombstone()) {
                    (true, true) => {
                        let winner = tombstone_winner(&rec_a, &rec_b, lossless_cap(&rec_a, &rec_b));
                        // Written to both sides, including the one that already
                        // holds the winning revision, so both carry the folded
                        // chain. `fold_ancestors` excludes the stored revision.
                        let la = overwrite(a, &winner, &rec_a.revision).await?;
                        let lb = overwrite(b, &winner, &rec_b.revision).await?;
                        if la && lb {
                            report.merged.push(key);
                        } else {
                            raced.push(key);
                        }
                    }
                    (true, false) | (false, true) => {
                        // Delete vs concurrent edit: no side wins without
                        // losing someone's intent (spec §5.4).
                        report.conflicts.push(SyncConflict {
                            key,
                            a: Box::new(rec_a),
                            b: Box::new(rec_b),
                        });
                    }
                    (false, false) => {
                        let base = ancestry_base(ancestry, &rec_a, &rec_b).await;
                        match merge(rec_a.kind.merge_class(), &base, &rec_a.body, &rec_b.body) {
                            MergeOutcome::Merged(body) => {
                                let merged = build_merged(&key, &rec_a, &rec_b, body);
                                let la = overwrite(a, &merged, &rec_a.revision).await?;
                                let lb = overwrite(b, &merged, &rec_b.revision).await?;
                                if la && lb {
                                    report.merged.push(key);
                                } else {
                                    // At least one side raced; re-loop to
                                    // reconcile the store that moved against
                                    // the now-merged peer.
                                    raced.push(key);
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
                },
            },
            (None, None) => {}
        }
    }
    Ok((report, raced))
}

/// Create `rec` in `dst`, where the raw read found the key absent, through the
/// replication write. Returns `false` if `dst` gained a record or a tombstone
/// since that read (`put_raw` → `Conflict`, nothing written), signalling the
/// caller to re-loop. `put_raw` never re-stamps, so a copy can't recreate over
/// a tombstone that arrived mid-pass.
async fn copy(dst: &dyn Store, rec: &Record) -> Result<bool> {
    Ok(matches!(
        dst.put_raw(rec.clone(), None).await?,
        PutResult::Committed(_)
    ))
}

/// Conditionally overwrite `dst` with `rec` (stored verbatim), expecting
/// revision `expected`, through the replication write. Returns `false` if a
/// concurrent mutation raced the write window: `Conflict` (the key moved, maybe
/// to a tombstone), or `NotFound` (the key was purged after sync's read).
async fn overwrite(dst: &dyn Store, rec: &Record, expected: &Revision) -> Result<bool> {
    match dst.put_raw(rec.clone(), Some(expected.clone())).await {
        Ok(PutResult::Committed(_)) => Ok(true),
        Ok(PutResult::Conflict(_)) => Ok(false),
        Err(CoreError::NotFound(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// The merged record for a diverged live-vs-live body merge (spec §3.4, §3.5),
/// via [`reconciled_record`](crate::tombstone::reconciled_record).
fn build_merged(key: &RecordKey, a: &Record, b: &Record, body: Body) -> Record {
    debug_assert_eq!(&a.key, key);
    debug_assert_eq!(&b.key, key);
    crate::tombstone::reconciled_record(
        a,
        b,
        body,
        Identity::new("gonzalo-sync"),
        "sync",
        lossless_cap(a, b),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memstore::MemStore;
    use crate::{CoreError, DeleteResult, Meta, PutResult, RecordKind, store::Conflict};
    use async_trait::async_trait;
    use std::collections::{BTreeMap, HashSet};
    use std::sync::Mutex;

    /// How a `RacyStore` interferes with writes. `put` and `put_raw` both
    /// consult it.
    // Test-only double: a short-lived value never stored in a collection, so
    // the size difference between variants doesn't matter.
    #[allow(clippy::large_enum_variant)]
    enum Race {
        /// One spurious `Conflict` on the first conditional write per key.
        FlakyOnce(Mutex<HashSet<RecordKey>>),
        /// Every write conflicts, except a create into a raw-absent key. A
        /// conditional write to a raw-absent key returns `Err(NotFound)`.
        Always,
        /// The first unconditional `put_raw` for this tombstone's key commits
        /// the tombstone first (a third peer's delete arriving mid-copy). Raw
        /// only.
        TombstoneArrives(Mutex<Option<Record>>),
    }

    /// A store whose writes are interfered with per `race`, delegating
    /// everything else to the reference `MemStore`. See [`Race`].
    struct RacyStore {
        inner: MemStore,
        race: Race,
    }

    impl RacyStore {
        /// A store that returns one spurious `Conflict` on the first
        /// conditional (`expected.is_some()`) write per key, like a concurrent
        /// writer racing the first overwrite, then behaves like the reference
        /// `MemStore`. Forces the sync re-loop to retry and still converge. It
        /// races both `put` and `put_raw`, so it behaves the same before and
        /// after sync moves to raw writes.
        fn flaky_once() -> Self {
            Self {
                inner: MemStore::new(),
                race: Race::FlakyOnce(Mutex::new(HashSet::new())),
            }
        }

        /// A store whose every write *always* races (a concurrent writer that
        /// never stops), except a create into a raw-absent key, which commits.
        /// Used to prove the re-loop is bounded and terminates. Races both
        /// `put` and `put_raw`.
        fn always() -> Self {
            Self {
                inner: MemStore::new(),
                race: Race::Always,
            }
        }

        /// A destination that a third peer's tombstone `arriving` reaches mid
        /// pass. The first unconditional `put_raw` for `arriving`'s key first
        /// commits that tombstone, then runs the copy. This lands exactly in
        /// the window between sync's `get_raw` (which saw the key absent) and
        /// its write. Raw only.
        fn tombstone_arrives(arriving: Record) -> Self {
            Self {
                inner: MemStore::new(),
                race: Race::TombstoneArrives(Mutex::new(Some(arriving))),
            }
        }

        /// Interference shared by `put` and `put_raw`. `raw` distinguishes them
        /// for `Race::TombstoneArrives`, which only interferes with `put_raw`.
        /// Returns `Some(result)` to short-circuit the write with that result;
        /// `None` lets the write proceed to the inner store unmodified.
        async fn interfere(
            &self,
            record: &Record,
            expected: &Option<Revision>,
            raw: bool,
        ) -> Result<Option<PutResult>> {
            match &self.race {
                Race::FlakyOnce(tripped) => {
                    let trip = expected.is_some() && {
                        let mut g = tripped.lock().unwrap();
                        g.insert(record.key.clone())
                    };
                    if trip && let Some(current) = self.inner.get_raw(&record.key).await? {
                        return Ok(Some(PutResult::Conflict(Box::new(Conflict {
                            key: record.key.clone(),
                            expected: expected.clone(),
                            current,
                        }))));
                    }
                    Ok(None)
                }
                Race::Always => match self.inner.get_raw(&record.key).await? {
                    None if expected.is_none() => Ok(None),
                    None => Err(CoreError::NotFound(record.key.clone())),
                    Some(current) => Ok(Some(PutResult::Conflict(Box::new(Conflict {
                        key: record.key.clone(),
                        expected: expected.clone(),
                        current,
                    })))),
                },
                Race::TombstoneArrives(slot) => {
                    if raw {
                        let arriving = if expected.is_none() {
                            let mut guard = slot.lock().unwrap();
                            match guard.as_ref() {
                                Some(t) if t.key == record.key => guard.take(),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        if let Some(t) = arriving {
                            let _ = self.inner.put_raw(t, None).await?;
                        }
                    }
                    Ok(None)
                }
            }
        }
    }

    #[async_trait]
    impl Store for RacyStore {
        async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.inner.get(key).await
        }
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            if let Some(raced) = self.interfere(&record, &expected, false).await? {
                return Ok(raced);
            }
            self.inner.put(record, expected).await
        }
        async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            if let Some(raced) = self.interfere(&record, &expected, true).await? {
                return Ok(raced);
            }
            self.inner.put_raw(record, expected).await
        }
        async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            self.inner.list(prefix).await
        }
        async fn delete_as(
            &self,
            key: &RecordKey,
            expected: Option<Revision>,
            author: Option<Identity>,
        ) -> Result<DeleteResult> {
            self.inner.delete_as(key, expected, author).await
        }
        async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.inner.get_raw(key).await
        }
        async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            self.inner.list_raw(prefix).await
        }
        async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
            self.inner.purge(key, expected).await
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
            ancestors: Vec::new(),
            deleted_at: None,
        }
    }

    #[tokio::test]
    async fn copies_one_sided_records_both_directions() {
        let a = MemStore::new();
        let b = MemStore::new();
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
        let a = MemStore::new();
        let b = MemStore::new();
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
        let a = MemStore::new();
        let b = MemStore::new();
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
        let a = MemStore::new();
        let b = MemStore::new();
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
        let a = MemStore::new();
        let b = MemStore::new();
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
    async fn session_with_blank_and_duplicate_lines_survives_divergent_sync() {
        // Regression for #133: a Session whose committed body legitimately holds
        // a blank line and a repeated line must survive a divergent sync intact.
        // `sync` merges against an empty base, so the old split+dedup path
        // silently dropped the blank line and collapsed the repeat, corrupting
        // BOTH stores (ADR 0005 violation). The merge must be side A verbatim
        // plus side B's divergent tail.
        let a = MemStore::new();
        let b = MemStore::new();
        let _ = a
            .put(
                rec("s", RecordKind::Session, "a\n\nyes\nyes\nfrom_a\n"),
                None,
            )
            .await
            .unwrap();
        let _ = b
            .put(
                rec("s", RecordKind::Session, "a\n\nyes\nyes\nfrom_b\n"),
                None,
            )
            .await
            .unwrap();

        let report = sync(&a, &b).await.unwrap();
        let key = RecordKey::new("ns", "col", "s");
        assert_eq!(report.merged, vec![key.clone()]);
        assert!(report.conflicts.is_empty());

        let ma = a.get(&key).await.unwrap().unwrap();
        let text = String::from_utf8(ma.body.bytes().to_vec()).unwrap();
        // Blank line preserved, "yes\nyes" not collapsed, both appends present.
        assert_eq!(text, "a\n\nyes\nyes\nfrom_a\nfrom_b\n");
        // Both stores converge to the same merged revision.
        let mb = b.get(&key).await.unwrap().unwrap();
        assert_eq!(ma.revision, mb.revision);
        assert_eq!(ma.body, mb.body);
    }

    #[tokio::test]
    async fn re_loops_until_a_racing_store_converges() {
        // B races the first overwrite (non-quiescent during the merge window).
        // A single pass would swallow that conflict and leave B un-synced; the
        // re-loop must retry until both stores converge.
        let a = MemStore::new();
        let b = RacyStore::flaky_once();
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
        // Pass 1 merged into A and lost the race on B. Pass 2 found B behind
        // A's chain and fast-forwarded it; the report is the last pass's.
        assert_eq!(report.fast_forwarded_to_b, vec![key]);
        assert!(report.merged.is_empty());
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
        let a = RacyStore::always();
        let b = RacyStore::always();
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

    fn k(id: &str) -> RecordKey {
        RecordKey::new("ns", "col", id)
    }

    async fn commit(store: &dyn Store, r: Record, expected: Option<Revision>) -> Revision {
        let PutResult::Committed(rev) = store.put(r, expected).await.unwrap() else {
            panic!("put did not commit");
        };
        rev
    }

    /// An ordinary application update of a live record: next revision, parent =
    /// current, expected = current. The store folds the replaced revision into
    /// `ancestors`.
    async fn edit(store: &dyn Store, id: &str, kind: RecordKind, payload: &str) -> Revision {
        let cur = store
            .get(&k(id))
            .await
            .unwrap()
            .expect("edit needs a live record");
        let mut r = rec(id, kind, payload);
        r.revision = cur.revision.next(payload.as_bytes());
        r.parent = Some(cur.revision.clone());
        commit(store, r, Some(cur.revision)).await
    }

    async fn delete(store: &dyn Store, id: &str, expected: Revision) -> Record {
        assert!(matches!(
            store.delete(&k(id), Some(expected)).await.unwrap(),
            DeleteResult::Deleted
        ));
        let t = store.get_raw(&k(id)).await.unwrap().unwrap();
        assert!(t.is_tombstone());
        t
    }

    // ---- §6.2: tombstones ----

    #[tokio::test]
    async fn stale_peer_takes_the_tombstone() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let tomb = delete(&a, "d", r0).await;

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.fast_forwarded_to_b, vec![k("d")]);
        assert!(report.conflicts.is_empty() && report.merged.is_empty());
        assert!(report.copied_to_a.is_empty() && report.copied_to_b.is_empty());
        assert!(
            b.get(&k("d")).await.unwrap().is_none(),
            "hidden from consumers"
        );
        let tb = b.get_raw(&k("d")).await.unwrap().unwrap();
        assert!(tb.is_tombstone());
        assert_eq!(tb.revision, tomb.revision);
        // Converged: a further sync does nothing.
        assert_eq!(sync(&a, &b).await.unwrap(), SyncReport::default());
    }

    #[tokio::test]
    async fn stale_peer_takes_the_tombstone_in_either_sync_direction() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let tomb = delete(&a, "d", r0).await;

        // Same state, arguments swapped: the deleted store is now side B.
        let report = sync(&b, &a).await.unwrap();

        assert_eq!(report.fast_forwarded_to_a, vec![k("d")]);
        assert!(report.conflicts.is_empty());
        assert!(b.get(&k("d")).await.unwrap().is_none());
        assert_eq!(
            b.get_raw(&k("d")).await.unwrap().unwrap().revision,
            tomb.revision
        );
        assert!(a.get(&k("d")).await.unwrap().is_none(), "never copied back");
        assert_eq!(
            a.get_raw(&k("d")).await.unwrap().unwrap().revision,
            tomb.revision
        );
    }

    #[tokio::test]
    async fn recreation_after_delete_propagates_live() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let _ = delete(&a, "d", r0).await;
        let _ = sync(&a, &b).await.unwrap();

        // Recreate with a fresh counter-0 record; the store re-stamps it.
        let r2 = commit(&a, rec("d", RecordKind::Topic, "again\n"), None).await;
        assert_eq!(r2.counter, 2);

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.fast_forwarded_to_b, vec![k("d")]);
        assert!(report.conflicts.is_empty());
        let live = b
            .get(&k("d"))
            .await
            .unwrap()
            .expect("recreated record is live on b");
        assert_eq!(live.revision, r2);
        assert_eq!(live.body.bytes(), b"again\n");
    }

    #[tokio::test]
    async fn delete_vs_concurrent_edit_is_a_conflict_and_writes_nothing() {
        // Topic is AppendOnly: if this reached the body merge it would "merge".
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let tomb = delete(&a, "d", r0).await;
        let edited = edit(&b, "d", RecordKind::Topic, "v0\nedit\n").await;

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.conflicts[0].key, k("d"));
        assert!(report.conflicts[0].a.is_tombstone());
        assert!(!report.conflicts[0].b.is_tombstone());
        assert!(report.merged.is_empty());
        assert!(report.fast_forwarded_to_a.is_empty() && report.fast_forwarded_to_b.is_empty());
        assert_eq!(
            a.get_raw(&k("d")).await.unwrap().unwrap().revision,
            tomb.revision
        );
        assert_eq!(b.get(&k("d")).await.unwrap().unwrap().revision, edited);
    }

    #[tokio::test]
    async fn edit_vs_concurrent_delete_on_b_is_a_conflict_and_writes_nothing() {
        // Mirror of `delete_vs_concurrent_edit_is_a_conflict_and_writes_nothing`
        // with the sides swapped: the edit is on A, the delete is on B. Topic is
        // AppendOnly: if this reached the body merge it would "merge".
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let edited = edit(&a, "d", RecordKind::Topic, "v0\nedit\n").await;
        let tomb = delete(&b, "d", r0).await;

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.conflicts[0].key, k("d"));
        assert!(!report.conflicts[0].a.is_tombstone());
        assert!(report.conflicts[0].b.is_tombstone());
        assert!(report.merged.is_empty());
        assert!(report.fast_forwarded_to_a.is_empty() && report.fast_forwarded_to_b.is_empty());
        // Neither store was written.
        assert_eq!(a.get(&k("d")).await.unwrap().unwrap().revision, edited);
        assert_eq!(
            b.get_raw(&k("d")).await.unwrap().unwrap().revision,
            tomb.revision
        );
    }

    #[tokio::test]
    async fn concurrent_tombstones_converge_on_the_higher_one() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        // A edits then deletes (tombstone counter 2); B deletes r0 (counter 1).
        let r1 = edit(&a, "d", RecordKind::Topic, "v0\nv1\n").await;
        let ta = delete(&a, "d", r1.clone()).await;
        let tb = delete(&b, "d", r0.clone()).await;
        assert_eq!(ta.revision.counter, 2);
        assert_eq!(tb.revision.counter, 1);

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.merged, vec![k("d")]);
        assert!(report.conflicts.is_empty());
        for store in [&a, &b] {
            let got = store.get_raw(&k("d")).await.unwrap().unwrap();
            assert!(got.is_tombstone());
            assert_eq!(got.revision, ta.revision);
            assert!(got.ancestors.contains(&tb.revision));
            assert!(got.ancestors.contains(&r1));
            assert!(got.ancestors.contains(&r0));
            assert!(!got.ancestors.contains(&ta.revision));
        }
        assert_eq!(sync(&a, &b).await.unwrap(), SyncReport::default());
    }

    #[tokio::test]
    async fn one_sided_tombstone_is_copied() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let tomb = delete(&a, "d", r0).await;

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.copied_to_b, vec![k("d")]);
        assert!(b.get(&k("d")).await.unwrap().is_none());
        assert_eq!(
            b.get_raw(&k("d")).await.unwrap().unwrap().revision,
            tomb.revision
        );
        assert_eq!(sync(&a, &b).await.unwrap(), SyncReport::default());
    }

    // ---- §6.2: ancestry-driven ordering for ordinary records ----

    #[tokio::test]
    async fn opaque_fast_forward_no_longer_conflicts() {
        let a = MemStore::new();
        let b = MemStore::new();
        let _ = commit(&a, rec("c", RecordKind::Checkpoint, "c0"), None).await;
        let _ = sync(&a, &b).await.unwrap();

        // A moves ahead; B is simply behind.
        let c1 = edit(&a, "c", RecordKind::Checkpoint, "c1").await;
        let report = sync(&a, &b).await.unwrap();
        assert!(report.conflicts.is_empty(), "behind is not diverged");
        assert_eq!(report.fast_forwarded_to_b, vec![k("c")]);
        assert_eq!(b.get(&k("c")).await.unwrap().unwrap().revision, c1);

        // And the other way round.
        let c2 = edit(&b, "c", RecordKind::Checkpoint, "c2").await;
        let report = sync(&a, &b).await.unwrap();
        assert!(report.conflicts.is_empty());
        assert_eq!(report.fast_forwarded_to_a, vec![k("c")]);
        assert_eq!(a.get(&k("c")).await.unwrap().unwrap().revision, c2);
    }

    #[tokio::test]
    async fn checkpoint_true_divergence_from_a_shared_parent_still_conflicts() {
        let a = MemStore::new();
        let b = MemStore::new();
        let _ = commit(&a, rec("c", RecordKind::Checkpoint, "c0"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let ca = edit(&a, "c", RecordKind::Checkpoint, "from_a").await;
        let cb = edit(&b, "c", RecordKind::Checkpoint, "from_b").await;

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.conflicts.len(), 1);
        assert!(report.fast_forwarded_to_a.is_empty() && report.fast_forwarded_to_b.is_empty());
        assert_eq!(a.get(&k("c")).await.unwrap().unwrap().revision, ca);
        assert_eq!(b.get(&k("c")).await.unwrap().unwrap().revision, cb);
    }

    #[tokio::test]
    async fn legacy_record_without_ancestors_takes_the_merge_path() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("l", RecordKind::Topic, "base\n"), None).await;
        // B holds a descendant of r0 written by a pre-0.7 binary: parent set,
        // ancestors empty. Put into an empty store, so nothing is folded in.
        let mut legacy = rec("l", RecordKind::Topic, "base\nmore\n");
        legacy.revision = r0.next(b"base\nmore\n");
        legacy.parent = Some(r0);
        let _ = commit(&b, legacy, None).await;
        assert!(
            b.get_raw(&k("l"))
                .await
                .unwrap()
                .unwrap()
                .ancestors
                .is_empty()
        );

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(
            report.merged,
            vec![k("l")],
            "no chain: merge, not fast-forward"
        );
        assert!(report.fast_forwarded_to_a.is_empty() && report.fast_forwarded_to_b.is_empty());
        assert_eq!(
            a.get(&k("l")).await.unwrap().unwrap().body.bytes(),
            b"base\nmore\n"
        );
    }

    #[tokio::test]
    async fn merge_folds_both_sides_ancestors() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("t", RecordKind::Topic, "base\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let ra = edit(&a, "t", RecordKind::Topic, "base\nfrom_a\n").await;
        let rb = edit(&b, "t", RecordKind::Topic, "base\nfrom_b\n").await;

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.merged, vec![k("t")]);
        let ma = a.get_raw(&k("t")).await.unwrap().unwrap();
        let mb = b.get_raw(&k("t")).await.unwrap().unwrap();
        assert_eq!(ma.revision, mb.revision);
        assert_eq!(ma.ancestors, mb.ancestors, "byte-identical on both sides");
        assert!(ma.ancestors.contains(&ra));
        assert!(ma.ancestors.contains(&rb));
        assert!(ma.ancestors.contains(&r0));
        assert!(!ma.ancestors.contains(&ma.revision));
    }

    #[tokio::test]
    async fn truncated_chain_never_overwrites() {
        let a = MemStore::new().with_ancestor_cap(2);
        let b = MemStore::new();
        let c0 = commit(&a, rec("c", RecordKind::Checkpoint, "c0"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        // Three edits on a cap-2 store push c0 out of A's chain.
        let _ = edit(&a, "c", RecordKind::Checkpoint, "c1").await;
        let _ = edit(&a, "c", RecordKind::Checkpoint, "c2").await;
        let _ = edit(&a, "c", RecordKind::Checkpoint, "c3").await;
        let ra = a.get_raw(&k("c")).await.unwrap().unwrap();
        assert_eq!(ra.ancestors.len(), 2);
        assert!(!ra.ancestors.contains(&c0));

        let report = sync(&a, &b).await.unwrap();

        // Fails safe: an unknown chain looks diverged (Opaque → conflict).
        assert_eq!(report.conflicts.len(), 1);
        assert!(report.fast_forwarded_to_b.is_empty());
        assert_eq!(b.get(&k("c")).await.unwrap().unwrap().revision, c0);
    }

    #[tokio::test]
    async fn mixed_ancestor_caps_converge() {
        let a = MemStore::new().with_ancestor_cap(4);
        let b = MemStore::new().with_ancestor_cap(32);
        let _ = commit(&a, rec("t", RecordKind::Topic, "0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();

        // B edits 10 times (chain of 10 fits in 32): B is ahead of A.
        for i in 1..=10 {
            let _ = edit(&b, "t", RecordKind::Topic, &format!("{i}\n")).await;
        }
        let report = sync(&a, &b).await.unwrap();
        assert!(report.conflicts.is_empty());
        assert_eq!(report.fast_forwarded_to_a, vec![k("t")]);
        assert_eq!(
            a.get_raw(&k("t")).await.unwrap().unwrap().ancestors.len(),
            4
        );

        // A edits twice: its 4-entry chain still holds B's revision.
        let _ = edit(&a, "t", RecordKind::Topic, "11\n").await;
        let r12 = edit(&a, "t", RecordKind::Topic, "12\n").await;
        let report = sync(&a, &b).await.unwrap();
        assert!(report.conflicts.is_empty());
        assert_eq!(report.fast_forwarded_to_b, vec![k("t")]);

        let ra = a.get_raw(&k("t")).await.unwrap().unwrap();
        let rb = b.get_raw(&k("t")).await.unwrap().unwrap();
        assert_eq!(ra.revision, r12);
        assert_eq!(rb.revision, r12);
        assert_eq!(ra.ancestors.len(), 4, "A truncates to its own cap");
        assert_eq!(
            rb.ancestors.len(),
            12,
            "B keeps r0..r11 under its larger cap"
        );
        assert_eq!(sync(&a, &b).await.unwrap(), SyncReport::default());
    }

    // ---- §6.2: racing doubles, tombstone variants ----

    #[tokio::test]
    async fn tombstone_reaches_a_peer_that_races_the_first_overwrite() {
        let a = MemStore::new();
        let b = RacyStore::flaky_once();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap(); // copy: unconditional, never trips
        let tomb = delete(&a, "d", r0).await;

        let report = sync(&a, &b).await.unwrap();

        assert!(report.conflicts.is_empty());
        assert_eq!(report.fast_forwarded_to_b, vec![k("d")]);
        let tb = b.get_raw(&k("d")).await.unwrap().unwrap();
        assert!(tb.is_tombstone());
        assert_eq!(tb.revision, tomb.revision);
    }

    #[tokio::test]
    async fn tombstone_sync_against_an_always_racing_peer_terminates() {
        let a = MemStore::new();
        let b = RacyStore::always();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = commit(&b, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let tomb = delete(&a, "d", r0.clone()).await;

        // Every overwrite of B races; sync must still return.
        let report = sync(&a, &b).await.unwrap();

        assert!(report.fast_forwarded_to_b.is_empty());
        assert!(report.conflicts.is_empty());
        assert_eq!(b.get(&k("d")).await.unwrap().unwrap().revision, r0);
        assert_eq!(
            a.get_raw(&k("d")).await.unwrap().unwrap().revision,
            tomb.revision
        );

        // The stores still disagree — B holds the live record, A the tombstone —
        // and `conflicts` is empty, so the report must say so itself (#290).
        assert!(
            !report.converged(),
            "an exhausted run must not look converged"
        );
        assert_eq!(report.unconverged, vec![k("d")]);
    }

    #[tokio::test]
    async fn a_settled_pair_reports_convergence() {
        // The ordinary case: one clean pass, so nothing is left racing.
        let a = MemStore::new();
        let b = MemStore::new();
        commit(&a, rec("x", RecordKind::Topic, "v0\n"), None).await;

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.copied_to_b, vec![k("x")]);
        assert!(report.converged());
        assert!(report.unconverged.is_empty());
    }

    #[tokio::test]
    async fn a_pass_that_settles_after_a_race_reports_convergence() {
        // B races the first overwrite, then behaves. The later clean pass
        // replaces the racing pass's whole report, so no stale unconverged
        // keys survive into the result.
        let a = MemStore::new();
        let b = RacyStore::flaky_once();
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
        assert!(report.converged(), "the retry pass landed cleanly");
        assert!(report.unconverged.is_empty());
    }

    // ---- write helpers treat store movement as a race ----

    #[tokio::test]
    async fn overwrite_treats_not_found_and_conflict_as_a_race() {
        let b = MemStore::new();
        // Absent (e.g. purged) key + Some(expected) → plan_put_raw NotFound.
        let stray = rec("x", RecordKind::Topic, "x\n");
        assert!(
            !overwrite(&b, &stray, &Revision::initial(b"gone"))
                .await
                .unwrap()
        );
        assert!(b.get_raw(&k("x")).await.unwrap().is_none());

        // Tombstoned key + Some(other) → plan_put_raw Conflict { current: t }.
        let r0 = commit(&b, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let tomb = delete(&b, "d", r0).await;
        let incoming = rec("d", RecordKind::Topic, "v1\n");
        assert!(
            !overwrite(&b, &incoming, &Revision::initial(b"stale"))
                .await
                .unwrap()
        );
        assert_eq!(
            b.get_raw(&k("d")).await.unwrap().unwrap().revision,
            tomb.revision
        );
    }

    #[tokio::test]
    async fn copy_over_a_tombstone_conflicts_and_writes_nothing() {
        let b = MemStore::new();
        let r0 = commit(&b, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let tomb = delete(&b, "d", r0).await;
        // As if the tombstone arrived after sync's raw read saw the key absent.
        assert!(
            !copy(&b, &rec("d", RecordKind::Topic, "v0\n"))
                .await
                .unwrap()
        );
        let still = b.get_raw(&k("d")).await.unwrap().unwrap();
        assert!(still.is_tombstone(), "put_raw never recreates");
        assert_eq!(still.revision, tomb.revision);
    }

    #[tokio::test]
    async fn tombstone_arriving_mid_copy_is_not_resurrected() {
        // A third peer C deleted r0. A still holds live r0. B is empty when
        // sync reads it, but C's tombstone reaches B between that raw read
        // and sync's copy of A's r0.
        let c = MemStore::new();
        let r0 = commit(&c, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let tomb = delete(&c, "d", r0.clone()).await;

        let a = MemStore::new();
        let a_r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        assert_eq!(a_r0, r0);
        let b = RacyStore::tombstone_arrives(tomb.clone());

        let report = sync(&a, &b).await.unwrap();

        // Pass 1: the copy conflicts on the arrived tombstone and re-loops.
        // Pass 2: A's r0 is in the tombstone's chain, so A fast-forwards.
        assert!(report.conflicts.is_empty());
        assert!(report.copied_to_b.is_empty());
        assert_eq!(report.fast_forwarded_to_a, vec![k("d")]);
        for store in [&a as &dyn Store, &b as &dyn Store] {
            assert!(
                store.get(&k("d")).await.unwrap().is_none(),
                "not resurrected"
            );
            let got = store.get_raw(&k("d")).await.unwrap().unwrap();
            assert!(got.is_tombstone());
            assert_eq!(got.revision, tomb.revision);
        }
        assert_eq!(sync(&a, &b).await.unwrap(), SyncReport::default());
    }
}
