//! Mark-sweep GC over content-addressed slices (ADR 0012, ticket A6): a blob is
//! live iff some live manifest references its hash; the rest are swept.

use gonzalo_core::{BlobStore, Manifest, gc_blobs};
use gonzalo_store_fs::FsStore;

fn fresh_store() -> FsStore {
    let dir = tempfile::tempdir().expect("tempdir");
    FsStore::new(dir.keep())
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

    let report = gc_blobs(&store, &[main, feature]).await.unwrap();

    assert_eq!(report.freed, vec![orphan.clone()]);
    assert_eq!(report.retained, 2);
    // The orphan is gone; both referenced slices survive.
    assert_eq!(store.get_blob(&orphan).await.unwrap(), None);
    assert!(store.get_blob(&live_a).await.unwrap().is_some());
    assert!(store.get_blob(&live_b).await.unwrap().is_some());
}

#[tokio::test]
async fn gc_with_no_live_manifests_frees_everything() {
    let store = fresh_store();
    store.put_blob(b"a").await.unwrap();
    store.put_blob(b"b").await.unwrap();

    let report = gc_blobs(&store, &[]).await.unwrap();
    assert_eq!(report.freed.len(), 2);
    assert_eq!(report.retained, 0);
    assert!(store.list_blobs().await.unwrap().is_empty());
}
