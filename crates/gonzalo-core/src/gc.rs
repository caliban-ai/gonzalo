//! Mark-sweep garbage collection for blobs (ADR 0012, gonzalo#292).
//!
//! A blob is *live* iff some stored record still points at it: as a record's
//! own [`Body::Blob`], as the blob a tombstone pins, as a slice a graph
//! manifest names, or as a shard a vector manifest names. GC marks that union
//! and sweeps every stored blob outside it. Liveness is derived from the
//! records themselves rather than a maintained refcount, so it is
//! self-correcting: a missed event can leave a blob briefly un-swept, never
//! wrongly deleted, and never leaked forever the way a drifted refcount would.

use crate::{BlobStore, Body, ContentHash, KeyPrefix, Manifest, Record, RecordKind, Result, Store};
use std::collections::BTreeSet;

/// What a GC sweep did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Hashes of blobs deleted because no record referenced them.
    pub freed: Vec<ContentHash>,
    /// Count of blobs kept because they are still referenced.
    pub retained: usize,
}

/// The slice hashes referenced by any of the `manifests` (ADR 0012).
///
/// This is one of the three sources [`live_blob_hashes`] unions; call that
/// instead when marking a whole store, or a manifest's slices would be the only
/// blobs kept and every record body swept.
pub fn live_slice_hashes<'a>(
    manifests: impl IntoIterator<Item = &'a Manifest>,
) -> BTreeSet<ContentHash> {
    manifests
        .into_iter()
        .flat_map(|m| m.entries.values().cloned())
        .collect()
}

/// The sweep set: hashes present in `all` but not in the live set, returned
/// sorted and deduplicated (`all - live`).
pub fn unreferenced_slices(all: &[ContentHash], live: &BTreeSet<ContentHash>) -> Vec<ContentHash> {
    let mut garbage: Vec<ContentHash> =
        all.iter().filter(|h| !live.contains(*h)).cloned().collect();
    garbage.sort();
    garbage.dedup();
    garbage
}

/// The mark set: every blob hash these records still need. `records` must be
/// the store's **raw** records, tombstones included.
///
/// Four things reference a blob (gonzalo#292, gonzalo#323):
///
/// - a record whose body is a [`Body::Blob`] — the bytes are its content;
/// - a tombstone's [`deleted_blob`](Record::deleted_blob). **A tombstone pins
///   its blob**, so a delete never destroys content that a peer can still
///   resurrect by syncing the record back, and re-putting the same content
///   after a delete does not have to re-upload it. The pin lasts exactly as
///   long as the tombstone: collecting it past the horizon releases the blob to
///   the next sweep;
/// - every slice a graph manifest names (ADR 0012) — blobs referenced from a
///   record's *contents* rather than from its body;
/// - every shard a vector manifest names (ADR 0027) — the same
///   contents-not-body reference, for a durable vector index's shard blobs.
///
/// Marking any one of the four alone deletes the other three's blobs, so this
/// unions them from the records rather than from a caller's idea of liveness.
pub fn live_blob_hashes<'a>(
    records: impl IntoIterator<Item = &'a Record>,
) -> Result<BTreeSet<ContentHash>> {
    let mut live = BTreeSet::new();
    for record in records {
        if let Body::Blob { hash, .. } = &record.body {
            live.insert(hash.clone());
        }
        if let Some(hash) = &record.deleted_blob {
            live.insert(hash.clone());
        }
        if record.kind == RecordKind::GraphManifest {
            live.extend(Manifest::from_body(&record.body)?.entries.into_values());
        }
        // A vector manifest's shards are live blobs. Without this the next sweep
        // deletes every vector in the index while the manifest still names them,
        // and — unlike a graph slice — they cannot be regenerated from source.
        if record.kind == RecordKind::VectorManifest {
            live.extend(
                crate::VectorManifest::from_body(&record.body)?
                    .entries
                    .into_values(),
            );
        }
    }
    Ok(live)
}

/// Delete every blob in `blobs` outside the `live` mark set, and report what
/// was freed versus retained.
///
/// Split from [`gc_blobs`] so a caller that already knows its live set — a
/// single view's manifests, say — can sweep without re-listing the store. The
/// mark set is the dangerous half: pass one that misses a reference and this
/// deletes content that no longer exists anywhere else.
pub async fn sweep_blobs<B>(blobs: &B, live: &BTreeSet<ContentHash>) -> Result<GcReport>
where
    B: BlobStore + ?Sized,
{
    let all = blobs.list_blobs().await?;
    let freed = unreferenced_slices(&all, live);
    for hash in &freed {
        blobs.delete_blob(hash).await?;
    }
    // `all` may repeat a hash (listing order and uniqueness are unspecified)
    // while `freed` is deduplicated, so count retained from the distinct set.
    let distinct: BTreeSet<&ContentHash> = all.iter().collect();
    let retained = distinct.len() - freed.len();
    Ok(GcReport { freed, retained })
}

/// Mark-sweep every blob in `store`: delete the ones no record references.
///
/// Explicit and operator-run, for the same reason collection is (ADR 0021): a
/// blob swept while a peer still holds the record naming it cannot be restored
/// by a later sync. Deleting a record does **not** free its blob — the
/// tombstone pins it — so reclaiming a deleted record's bytes is three steps:
/// `delete`, then `collect` past the horizon, then this.
pub async fn gc_blobs<T>(store: &T) -> Result<GcReport>
where
    T: Store + BlobStore + ?Sized,
{
    // Raw, because a tombstone is what pins a deleted record's blob. A consumer
    // listing hides tombstones, and sweeping against it would free exactly the
    // blobs the pin exists to keep.
    let keys = store.list_raw(&KeyPrefix::default()).await?;
    let mut records = Vec::with_capacity(keys.len());
    for key in &keys {
        if let Some(record) = store.get_raw(key).await? {
            records.push(record);
        }
    }
    sweep_blobs(store, &live_blob_hashes(&records)?).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Identity, Meta, RecordKey};

    fn h(s: &str) -> ContentHash {
        ContentHash::of(s.as_bytes())
    }

    fn meta() -> Meta {
        Meta::new(Identity::new("tester"), "test")
    }

    fn record(id: &str, kind: RecordKind, body: Body) -> Record {
        Record::create(RecordKey::new("ns", "coll", id), kind, body, meta())
    }

    fn blob_body(content: &str) -> Body {
        Body::blob(content.as_bytes())
    }

    #[test]
    fn live_set_unions_all_manifest_references() {
        let mut a = Manifest::new();
        a.insert("x.rs", h("1"));
        a.insert("y.rs", h("2"));
        let mut b = Manifest::new();
        b.insert("z.rs", h("2")); // shared slice, counted once
        b.insert("w.rs", h("3"));

        let live = live_slice_hashes([&a, &b]);
        assert_eq!(live, BTreeSet::from([h("1"), h("2"), h("3")]));
    }

    #[test]
    fn live_set_of_no_manifests_is_empty() {
        assert!(live_slice_hashes([]).is_empty());
    }

    #[test]
    fn unreferenced_is_all_minus_live_sorted() {
        let all = vec![h("keep"), h("drop"), h("keep2")];
        let live = BTreeSet::from([h("keep"), h("keep2")]);
        let garbage = unreferenced_slices(&all, &live);
        let mut want = vec![h("drop")];
        want.sort();
        assert_eq!(garbage, want);
    }

    #[test]
    fn unreferenced_dedups_repeated_input_hashes() {
        let all = vec![h("dup"), h("dup"), h("live")];
        let live = BTreeSet::from([h("live")]);
        assert_eq!(unreferenced_slices(&all, &live), vec![h("dup")]);
    }

    #[test]
    fn nothing_unreferenced_when_all_are_live() {
        let all = vec![h("a"), h("b")];
        let live = BTreeSet::from([h("a"), h("b")]);
        assert!(unreferenced_slices(&all, &live).is_empty());
    }

    #[test]
    fn mark_set_keeps_a_records_own_blob_body() {
        let r = record("doc", RecordKind::Checkpoint, blob_body("payload"));
        assert_eq!(
            live_blob_hashes([&r]).unwrap(),
            BTreeSet::from([h("payload")])
        );
    }

    #[test]
    fn mark_set_ignores_inline_bodies() {
        let r = record("t", RecordKind::Topic, Body::Inline(b"inline".to_vec()));
        assert!(live_blob_hashes([&r]).unwrap().is_empty());
    }

    #[test]
    fn mark_set_keeps_the_blob_a_tombstone_pins() {
        let mut t = record("doc", RecordKind::Tombstone, Body::Inline(Vec::new()));
        t.deleted_at = Some(1);
        t.deleted_blob = Some(h("payload"));
        assert_eq!(
            live_blob_hashes([&t]).unwrap(),
            BTreeSet::from([h("payload")])
        );
    }

    #[test]
    fn mark_set_keeps_slices_a_manifest_names() {
        // The slices are referenced by the manifest's *contents*, not by any
        // record body — marking bodies alone would sweep every code slice.
        let mut m = Manifest::new();
        m.insert("src/lib.rs", h("slice"));
        let r = record("view", RecordKind::GraphManifest, m.to_body());
        assert_eq!(
            live_blob_hashes([&r]).unwrap(),
            BTreeSet::from([h("slice")])
        );
    }

    #[test]
    fn mark_set_unions_bodies_pins_and_slices() {
        let live_record = record("doc", RecordKind::Checkpoint, blob_body("body"));
        let mut tombstone = record("gone", RecordKind::Tombstone, Body::Inline(Vec::new()));
        tombstone.deleted_at = Some(1);
        tombstone.deleted_blob = Some(h("pinned"));
        let mut m = Manifest::new();
        m.insert("src/lib.rs", h("slice"));
        let manifest = record("view", RecordKind::GraphManifest, m.to_body());

        let live = live_blob_hashes([&live_record, &tombstone, &manifest]).unwrap();
        assert_eq!(live, BTreeSet::from([h("body"), h("pinned"), h("slice")]));
    }

    #[test]
    fn mark_set_reports_an_undecodable_manifest_rather_than_sweeping_it() {
        // Silently treating a corrupt manifest as "references nothing" would
        // delete every slice it named.
        let r = record(
            "view",
            RecordKind::GraphManifest,
            Body::Inline(b"{[".to_vec()),
        );
        assert!(live_blob_hashes([&r]).is_err());
    }

    // Guard test, symmetric to
    // `mark_set_reports_an_undecodable_manifest_rather_than_sweeping_it`
    // above: silently treating an undecodable `VectorManifest` body as
    // "references nothing" would sweep every shard it actually named. Passes
    // against current code by design -- the `VectorManifest` arm already
    // propagates `VectorManifest::from_body`'s error via `?` rather than
    // swallowing it with `.ok()`; this guards against someone changing that
    // later.
    #[test]
    fn mark_set_reports_an_undecodable_vector_manifest_rather_than_sweeping_it() {
        let r = record(
            "memories",
            RecordKind::VectorManifest,
            Body::Inline(b"not json at all".to_vec()),
        );
        assert!(live_blob_hashes([&r]).is_err());
    }

    // Red-first guard on real data loss: before the VectorManifest arm exists,
    // the mark set misses every shard blob and a sweep deletes live vectors.
    #[test]
    fn live_set_includes_vector_manifest_shards() {
        use crate::VectorManifest;

        let mut vm = VectorManifest::new("bge-small-en-v1.5", 384, 256);
        vm.entries.insert(0, h("shard-0"));
        vm.entries.insert(9, h("shard-9"));

        let rec = record("memories", RecordKind::VectorManifest, vm.to_body());

        let live = live_blob_hashes([&rec]).unwrap();
        assert!(live.contains(&h("shard-0")));
        assert!(live.contains(&h("shard-9")));
    }

    #[test]
    fn a_graph_manifest_and_a_vector_manifest_both_mark() {
        use crate::VectorManifest;

        let mut gm = Manifest::new();
        gm.insert("src/lib.rs", h("slice"));
        let graph = record("view", RecordKind::GraphManifest, gm.to_body());

        let mut vm = VectorManifest::new("space", 8, 4);
        vm.entries.insert(1, h("shard"));
        let vector = record("memories", RecordKind::VectorManifest, vm.to_body());

        let live = live_blob_hashes([&graph, &vector]).unwrap();
        assert_eq!(live, BTreeSet::from([h("slice"), h("shard")]));
    }

    /// A `BlobStore` whose `list_blobs` returns a fixed, possibly-duplicated
    /// list of hashes and records which hashes `delete_blob` was called on.
    #[derive(Default)]
    struct FakeBlobs {
        listed: Vec<ContentHash>,
        deleted: std::sync::Mutex<Vec<ContentHash>>,
    }

    #[async_trait::async_trait]
    impl BlobStore for FakeBlobs {
        async fn put_blob(&self, content: &[u8]) -> Result<ContentHash> {
            Ok(ContentHash::of(content))
        }
        async fn get_blob(&self, _hash: &ContentHash) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
        async fn list_blobs(&self) -> Result<Vec<ContentHash>> {
            Ok(self.listed.clone())
        }
        async fn delete_blob(&self, hash: &ContentHash) -> Result<()> {
            self.deleted.lock().unwrap().push(hash.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn retained_counts_distinct_blobs_despite_duplicate_listing() {
        // `list_blobs` reports `keep` twice and one unreferenced `drop`. There
        // are two distinct blobs; one is freed, so exactly one is retained — the
        // duplicate listing must not inflate `retained` to 2 (regression: #156).
        let blobs = FakeBlobs {
            listed: vec![h("keep"), h("keep"), h("drop")],
            deleted: Default::default(),
        };

        let report = sweep_blobs(&blobs, &BTreeSet::from([h("keep")]))
            .await
            .unwrap();

        assert_eq!(report.freed, vec![h("drop")]);
        assert_eq!(report.retained, 1);
        assert_eq!(*blobs.deleted.lock().unwrap(), vec![h("drop")]);
    }

    #[tokio::test]
    async fn sweep_deletes_nothing_when_every_blob_is_live() {
        let blobs = FakeBlobs {
            listed: vec![h("a"), h("b")],
            deleted: Default::default(),
        };

        let report = sweep_blobs(&blobs, &BTreeSet::from([h("a"), h("b")]))
            .await
            .unwrap();

        assert!(report.freed.is_empty());
        assert_eq!(report.retained, 2);
        assert!(blobs.deleted.lock().unwrap().is_empty());
    }
}
