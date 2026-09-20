//! Mark-sweep GC over blobs (ADR 0012, gonzalo#292): a blob is live iff some
//! stored record still references it — as a record's own body, as the blob a
//! tombstone pins, or as a slice a graph manifest names. The rest are swept.

use gonzalo_core::{
    BlobStore, Body, ContentHash, DeleteResult, Identity, KeyPrefix, Manifest, Meta, PutResult,
    Record, RecordKey, RecordKind, Store, collect, gc_blobs, now_ms,
};
use gonzalo_store_fs::FsStore;
use std::time::Duration;

fn fresh_store() -> FsStore {
    let dir = tempfile::tempdir().expect("tempdir");
    FsStore::new(dir.keep())
}

fn meta() -> Meta {
    Meta::new(Identity::new("tester"), "test")
}

/// Store `record`, asserting the write committed rather than conflicted.
async fn committed(store: &FsStore, record: Record) {
    match store.put(record, None).await.expect("put") {
        PutResult::Committed(_) => {}
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    }
}

/// Store `manifest` as the `(repo, view)` view's `graph-manifest` record — the
/// shape GC reads slices out of.
async fn put_manifest(store: &FsStore, repo: &str, view: &str, manifest: &Manifest) {
    let record = Record::create(
        Manifest::key(repo, view),
        RecordKind::GraphManifest,
        manifest.to_body(),
        meta(),
    );
    committed(store, record).await;
}

/// Store `content` as a blob and a record whose body references it, returning
/// the blob's hash.
async fn put_blob_backed(store: &FsStore, key: &RecordKey, content: &[u8]) -> ContentHash {
    let hash = store.put_blob(content).await.expect("put blob");
    committed(
        store,
        Record::create(key.clone(), RecordKind::Topic, Body::blob(content), meta()),
    )
    .await;
    hash
}

#[tokio::test]
async fn list_blobs_reports_stored_hashes_and_skips_temps() {
    let store = fresh_store();
    let h1 = store.put_blob(b"one").await.unwrap();
    let h2 = store.put_blob(b"two").await.unwrap();

    let mut listed = store.list_blobs().await.unwrap();
    listed.sort();
    let mut want = vec![h1, h2];
    want.sort();
    assert_eq!(listed, want);
}

#[tokio::test]
async fn list_blobs_empty_when_no_blobs_written() {
    let store = fresh_store();
    assert!(store.list_blobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn delete_blob_removes_content_and_is_idempotent() {
    let store = fresh_store();
    let hash = store.put_blob(b"gone").await.unwrap();
    assert!(store.get_blob(&hash).await.unwrap().is_some());

    store.delete_blob(&hash).await.unwrap();
    assert_eq!(store.get_blob(&hash).await.unwrap(), None);
    // Deleting an already-absent blob is a no-op, not an error.
    store.delete_blob(&hash).await.unwrap();
}

#[tokio::test]
async fn gc_sweeps_slices_no_live_manifest_references() {
    let store = fresh_store();
    // Three slices stored; two are referenced by live manifests, one is orphaned.
    let live_a = store
        .put_blob(b"slice referenced by view main")
        .await
        .unwrap();
    let live_b = store
        .put_blob(b"slice referenced by view feature")
        .await
        .unwrap();
    let orphan = store
        .put_blob(b"slice no view references anymore")
        .await
        .unwrap();

    let mut main = Manifest::new();
    main.insert("src/lib.rs", live_a.clone());
    let mut feature = Manifest::new();
    feature.insert("src/mod.rs", live_b.clone());
    // live_a is also shared into the feature view — still one live reference is enough.
    feature.insert("src/lib.rs", live_a.clone());
    put_manifest(&store, "repo", "main", &main).await;
    put_manifest(&store, "repo", "feature", &feature).await;

    let report = gc_blobs(&store).await.unwrap();

    assert_eq!(report.freed, vec![orphan.clone()]);
    assert_eq!(report.retained, 2);
    // The orphan is gone; both referenced slices survive.
    assert_eq!(store.get_blob(&orphan).await.unwrap(), None);
    assert!(store.get_blob(&live_a).await.unwrap().is_some());
    assert!(store.get_blob(&live_b).await.unwrap().is_some());
}

#[tokio::test]
async fn gc_with_no_records_frees_everything() {
    let store = fresh_store();
    store.put_blob(b"a").await.unwrap();
    store.put_blob(b"b").await.unwrap();

    let report = gc_blobs(&store).await.unwrap();
    assert_eq!(report.freed.len(), 2);
    assert_eq!(report.retained, 0);
    assert!(store.list_blobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn gc_keeps_a_live_records_own_blob_body() {
    // Before #292 the mark set was built from manifests alone, so a record
    // whose body is a blob had its content swept out from under it.
    let store = fresh_store();
    let key = RecordKey::new("ns", "docs", "readme");
    let hash = put_blob_backed(&store, &key, b"the document body").await;

    let report = gc_blobs(&store).await.unwrap();

    assert!(
        report.freed.is_empty(),
        "a live record's blob is not garbage"
    );
    assert_eq!(report.retained, 1);
    assert!(store.get_blob(&hash).await.unwrap().is_some());
}

#[tokio::test]
async fn a_tombstone_pins_its_blob_until_it_is_collected() {
    let store = fresh_store();
    let key = RecordKey::new("ns", "docs", "readme");
    let hash = put_blob_backed(&store, &key, b"the document body").await;

    assert_eq!(
        store.delete_as(&key, None, None).await.unwrap(),
        DeleteResult::Deleted
    );

    // The record is gone from consumer reads, but its bytes are pinned: a peer
    // can still sync the record back, and re-putting the same content must not
    // have to re-upload it.
    let report = gc_blobs(&store).await.unwrap();
    assert!(
        report.freed.is_empty(),
        "a tombstone pins the blob of the record it replaced"
    );
    assert!(store.get_blob(&hash).await.unwrap().is_some());

    // Collecting the tombstone past the horizon releases the pin.
    let collected = collect(&store, &KeyPrefix::default(), Duration::ZERO, now_ms() + 1)
        .await
        .unwrap();
    assert_eq!(collected.purged.len(), 1);

    let report = gc_blobs(&store).await.unwrap();
    assert_eq!(report.freed, vec![hash.clone()]);
    assert_eq!(store.get_blob(&hash).await.unwrap(), None);
}

#[tokio::test]
async fn recreating_a_deleted_key_keeps_the_new_blob_and_drops_the_old_pin() {
    // Putting over a tombstone clears `deleted_blob`, so the old content stops
    // being pinned the moment the key is live again.
    let store = fresh_store();
    let key = RecordKey::new("ns", "docs", "readme");
    let old = put_blob_backed(&store, &key, b"first body").await;
    assert_eq!(
        store.delete_as(&key, None, None).await.unwrap(),
        DeleteResult::Deleted
    );

    let new = store.put_blob(b"second body").await.unwrap();
    committed(
        &store,
        Record::create(
            key.clone(),
            RecordKind::Topic,
            Body::blob(b"second body"),
            meta(),
        ),
    )
    .await;

    let report = gc_blobs(&store).await.unwrap();

    assert_eq!(report.freed, vec![old.clone()]);
    assert_eq!(store.get_blob(&old).await.unwrap(), None);
    assert!(store.get_blob(&new).await.unwrap().is_some());
}
