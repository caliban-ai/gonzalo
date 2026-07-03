//! `BlobStore` behavior for the fs substrate: content-addressed, write-if-absent,
//! deduping (ADR 0012, ticket A1).

use gonzalo_core::conformance::run_blob_store_conformance;
use gonzalo_core::{
    BlobStore, Body, ContentHash, Identity, Meta, PutResult, Record, RecordKey, RecordKind,
    Revision, Store,
};
use gonzalo_store_fs::FsStore;
use std::collections::BTreeMap;

fn fresh_store() -> FsStore {
    let dir = tempfile::tempdir().expect("tempdir");
    // Leak the TempDir so the directory outlives the store for one test.
    FsStore::new(dir.keep())
}

#[tokio::test]
async fn put_blob_is_content_addressed_and_get_roundtrips() {
    let store = fresh_store();
    let hash = store.put_blob(b"fn main() {}").await.unwrap();
    assert_eq!(hash, ContentHash::of(b"fn main() {}"));
    assert_eq!(
        store.get_blob(&hash).await.unwrap().as_deref(),
        Some(&b"fn main() {}"[..])
    );
}

#[tokio::test]
async fn get_blob_absent_returns_none() {
    let store = fresh_store();
    assert_eq!(
        store.get_blob(&ContentHash::of(b"nope")).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn put_blob_dedups_identical_content_to_one_artifact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.keep();
    let store = FsStore::new(&root);
    // The same slice content arriving from two views / paths must store once.
    let h1 = store.put_blob(b"shared slice").await.unwrap();
    let h2 = store.put_blob(b"shared slice").await.unwrap();
    assert_eq!(h1, h2);
    let count = std::fs::read_dir(root.join("blobs")).unwrap().count();
    assert_eq!(count, 1, "identical content must dedup to a single blob");
}

#[tokio::test]
async fn fs_passes_blob_store_conformance() {
    run_blob_store_conformance(|| async { fresh_store() }).await;
}

#[tokio::test]
async fn record_with_blob_body_roundtrips_through_store() {
    let store = fresh_store();
    // A blob-bodied record: the slice content lives in the blob store, the
    // record carries only the reference.
    let content = b"symbols + references for one file";
    let hash = store.put_blob(content).await.unwrap();
    let body = Body::blob(content);
    assert_eq!(
        body,
        Body::Blob {
            hash: hash.clone(),
            len: content.len() as u64
        }
    );

    let key = RecordKey::new("caliban", "graph-slices", &hash.0);
    let rec = Record {
        revision: Revision::initial(body.bytes()),
        parent: None,
        body,
        kind: RecordKind::Checkpoint,
        meta: Meta {
            author: Identity::new("tester"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        key: key.clone(),
    };
    let PutResult::Committed(rev) = store.put(rec.clone(), None).await.unwrap() else {
        panic!("expected Committed");
    };
    assert_eq!(rev, rec.revision);
    assert_eq!(store.get(&key).await.unwrap(), Some(rec));
    // The referenced content is still fetchable via the blob store.
    assert_eq!(
        store.get_blob(&hash).await.unwrap().as_deref(),
        Some(&content[..])
    );
}
