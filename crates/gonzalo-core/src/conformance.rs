//! A reusable conformance suite every `Store` impl must pass. Substrate
//! crates call `run_store_conformance(factory)` from their integration
//! tests. The factory returns a fresh, empty store per invocation.

use crate::{
    BlobStore, Body, ContentHash, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey,
    RecordKind, Revision, Store,
};
use std::collections::BTreeMap;

fn sample(key: RecordKey, payload: &[u8]) -> Record {
    let body = Body::Inline(payload.to_vec());
    Record {
        revision: Revision::initial(body.bytes()),
        parent: None,
        body,
        kind: RecordKind::Topic,
        meta: Meta {
            author: Identity::new("tester"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        key,
    }
}

/// Run the full suite against a store produced by `factory`.
pub async fn run_store_conformance<S, F, Fut>(factory: F)
where
    S: Store,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = S>,
{
    get_absent_returns_none(&factory().await).await;
    put_then_get_roundtrips(&factory().await).await;
    stale_expected_returns_conflict(&factory().await).await;
    list_filters_by_prefix(&factory().await).await;
}

async fn get_absent_returns_none<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "col", "missing");
    assert_eq!(store.get(&key).await.unwrap(), None);
}

async fn put_then_get_roundtrips<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "col", "a");
    let rec = sample(key.clone(), b"hello");
    let PutResult::Committed(committed_rev) = store.put(rec.clone(), None).await.unwrap() else {
        panic!("expected Committed");
    };
    assert_eq!(committed_rev, rec.revision);
    assert_eq!(store.get(&key).await.unwrap(), Some(rec));
}

async fn stale_expected_returns_conflict<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "col", "b");
    let first = sample(key.clone(), b"v1");
    let committed = match store.put(first.clone(), None).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("unexpected conflict on create"),
    };

    // A second writer who never saw `committed` tries to create again.
    let stale = sample(key.clone(), b"v2-from-stale-writer");
    match store.put(stale, None).await.unwrap() {
        PutResult::Conflict(c) => {
            assert_eq!(c.key, key);
            assert_eq!(c.current.revision, committed);
        }
        PutResult::Committed(_) => panic!("expected conflict for stale write"),
    }
}

/// Run the full blob-store suite against a store produced by `factory`
/// (a fresh, empty [`BlobStore`] per invocation).
pub async fn run_blob_store_conformance<B, F, Fut>(factory: F)
where
    B: BlobStore,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = B>,
{
    blob_get_absent_returns_none(&factory().await).await;
    blob_put_then_get_roundtrips(&factory().await).await;
    blob_put_is_content_addressed_and_idempotent(&factory().await).await;
    blob_list_reports_stored_hashes(&factory().await).await;
    blob_delete_removes_and_is_idempotent(&factory().await).await;
}

async fn blob_get_absent_returns_none<B: BlobStore>(store: &B) {
    assert_eq!(
        store.get_blob(&ContentHash::of(b"absent")).await.unwrap(),
        None
    );
}

async fn blob_put_then_get_roundtrips<B: BlobStore>(store: &B) {
    let content = b"symbols + references for one file";
    let hash = store.put_blob(content).await.unwrap();
    assert_eq!(hash, ContentHash::of(content));
    assert_eq!(
        store.get_blob(&hash).await.unwrap().as_deref(),
        Some(&content[..])
    );
}

async fn blob_put_is_content_addressed_and_idempotent<B: BlobStore>(store: &B) {
    let content = b"deterministic slice body";
    // Storing the same content twice yields the same hash and never conflicts.
    let first = store.put_blob(content).await.unwrap();
    let second = store.put_blob(content).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(first, ContentHash::of(content));
    // The content is still intact after the second (no-op) write.
    assert_eq!(
        store.get_blob(&first).await.unwrap().as_deref(),
        Some(&content[..])
    );
}

async fn blob_list_reports_stored_hashes<B: BlobStore>(store: &B) {
    assert!(store.list_blobs().await.unwrap().is_empty());
    let h1 = store.put_blob(b"slice one").await.unwrap();
    let h2 = store.put_blob(b"slice two").await.unwrap();
    let mut listed = store.list_blobs().await.unwrap();
    listed.sort();
    let mut want = vec![h1, h2];
    want.sort();
    assert_eq!(listed, want);
}

async fn blob_delete_removes_and_is_idempotent<B: BlobStore>(store: &B) {
    let hash = store.put_blob(b"to be collected").await.unwrap();
    assert!(store.get_blob(&hash).await.unwrap().is_some());
    store.delete_blob(&hash).await.unwrap();
    assert_eq!(store.get_blob(&hash).await.unwrap(), None);
    // Deleting an absent blob succeeds (idempotent).
    store.delete_blob(&hash).await.unwrap();
}

async fn list_filters_by_prefix<S: Store>(store: &S) {
    let r1 = store
        .put(sample(RecordKey::new("x", "c1", "1"), b"1"), None)
        .await
        .unwrap();
    assert!(matches!(r1, PutResult::Committed(_)));
    let r2 = store
        .put(sample(RecordKey::new("x", "c2", "2"), b"2"), None)
        .await
        .unwrap();
    assert!(matches!(r2, PutResult::Committed(_)));
    let prefix = KeyPrefix {
        namespace: Some("x".into()),
        collection: Some("c1".into()),
    };
    let mut keys = store.list(&prefix).await.unwrap();
    keys.sort();
    assert_eq!(keys, vec![RecordKey::new("x", "c1", "1")]);
}
