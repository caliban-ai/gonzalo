//! A reusable conformance suite every `Store` impl must pass. Substrate
//! crates call `run_store_conformance(factory)` from their integration
//! tests. The factory returns a fresh, empty store per invocation.

use crate::{
    BlobStore, Body, ContentHash, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record,
    RecordKey, RecordKind, Revision, Store,
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
    update_commits_then_stale_update_conflicts(&factory().await).await;
    list_filters_by_prefix(&factory().await).await;
    special_char_keys_dont_collide(&factory().await).await;
    delete_unconditional_removes(&factory().await).await;
    delete_absent_is_idempotent(&factory().await).await;
    delete_stale_expected_conflicts(&factory().await).await;
    delete_matching_expected_removes(&factory().await).await;
}

/// (a) A `put` then an unconditional `delete` (`expected = None`) removes the
/// record: the follow-up `get` returns `None`.
async fn delete_unconditional_removes<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "del", "unconditional");
    let rec = sample(key.clone(), b"bye");
    assert!(matches!(
        store.put(rec, None).await.unwrap(),
        PutResult::Committed(_)
    ));
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );
    assert_eq!(store.get(&key).await.unwrap(), None);
}

/// (b) Deleting an absent key with `expected = None` is an idempotent `Deleted`.
async fn delete_absent_is_idempotent<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "del", "absent");
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );
}

/// (c) A conditional `delete` with the wrong expected revision is a `Conflict`
/// whose `current.revision` is the live revision, and the record still exists.
async fn delete_stale_expected_conflicts<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "del", "stale");
    let rec = sample(key.clone(), b"keep");
    let rev = match store.put(rec, None).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("unexpected conflict on create"),
    };
    let wrong = Revision::initial(b"a-revision-that-was-never-current");
    assert_ne!(wrong, rev);
    match store.delete(&key, Some(wrong)).await.unwrap() {
        DeleteResult::Conflict(c) => {
            assert_eq!(c.key, key);
            assert_eq!(c.current.revision, rev);
        }
        DeleteResult::Deleted => panic!("stale conditional delete must conflict"),
    }
    // The record survived the rejected delete.
    assert_eq!(store.get(&key).await.unwrap().unwrap().revision, rev);
}

/// (d) A conditional `delete` with the matching expected revision removes it.
async fn delete_matching_expected_removes<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "del", "matching");
    let rec = sample(key.clone(), b"gone");
    let rev = match store.put(rec, None).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("unexpected conflict on create"),
    };
    assert_eq!(
        store.delete(&key, Some(rev)).await.unwrap(),
        DeleteResult::Deleted
    );
    assert_eq!(store.get(&key).await.unwrap(), None);
}

/// Distinct keys that mapped to the *same* physical path under the old lossy
/// `_`-collapse (`.` and `/` both became `_`) must now be independent records:
/// no cross-key overwrite, no spurious OCC conflict, and `list()` must return
/// each original key verbatim (encode/decode round-trip).
async fn special_char_keys_dont_collide<S: Store>(store: &S) {
    let dotted = RecordKey::new("ns", "col", "v1.0");
    let under = RecordKey::new("ns", "col", "v1_0");
    let slashy = RecordKey::new("a/b", "c.d", "e/f");

    // Creating all three with `expected = None` must each Commit — under the
    // old collision, `under` would see `dotted` already present and Conflict.
    for (k, payload) in [
        (&dotted, b"dotted".as_slice()),
        (&under, b"under".as_slice()),
        (&slashy, b"slashy".as_slice()),
    ] {
        match store.put(sample(k.clone(), payload), None).await.unwrap() {
            PutResult::Committed(_) => {}
            PutResult::Conflict(_) => panic!("distinct key {k:?} collided onto an existing record"),
        }
    }

    // Each retrievable independently with its own body — no clobber.
    for (k, payload) in [
        (&dotted, b"dotted".as_slice()),
        (&under, b"under".as_slice()),
        (&slashy, b"slashy".as_slice()),
    ] {
        let got = store.get(k).await.unwrap().expect("record present");
        assert_eq!(got.body.bytes(), payload, "wrong body for {k:?}");
    }

    // `list()` round-trips the exact keys (decode is the inverse of encode).
    let keys = store.list(&KeyPrefix::default()).await.unwrap();
    for k in [&dotted, &under, &slashy] {
        assert!(keys.contains(k), "list() missing {k:?}; got {keys:?}");
    }
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

async fn update_commits_then_stale_update_conflicts<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "col", "upd");

    // Create v1.
    let v1 = sample(key.clone(), b"v1");
    let rev1 = match store.put(v1, None).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("unexpected conflict on create"),
    };

    // Update with the correct `expected` revision commits.
    let mut v2 = sample(key.clone(), b"v2");
    v2.parent = Some(rev1.clone());
    v2.revision = rev1.next(b"v2");
    let rev2 = match store.put(v2, Some(rev1.clone())).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("update with correct expected must commit"),
    };
    assert_ne!(rev2, rev1, "an update produces a new revision");

    // A writer who still holds `rev1` tries to update again: conflict against
    // the now-current `rev2`. On a store with native conditional writes this is
    // enforced atomically at the object level (`If-Match`), not just by the
    // pre-read — closing the read-then-write TOCTOU.
    let mut stale = sample(key.clone(), b"v3-from-stale-writer");
    stale.parent = Some(rev1.clone());
    stale.revision = rev1.next(b"v3");
    match store.put(stale, Some(rev1)).await.unwrap() {
        PutResult::Conflict(c) => {
            assert_eq!(c.key, key);
            assert_eq!(c.current.revision, rev2);
        }
        PutResult::Committed(_) => panic!("stale update must conflict"),
    }

    // The winning value is still readable and is v2.
    assert_eq!(store.get(&key).await.unwrap().unwrap().revision, rev2);
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
