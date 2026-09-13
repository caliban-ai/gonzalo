//! fs-specific tombstone behaviour (gonzalo#203, spec §3.3): where tombstones
//! live on disk, how `purge` unlinks them, and how the ancestor cap is wired.
//! The substrate-independent semantics are covered by
//! `run_tombstone_conformance` in `tests/conformance.rs`.

use gonzalo_core::{
    Body, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind,
    Revision, Store,
};
use gonzalo_store_fs::FsStore;
use std::collections::BTreeMap;

fn rec(key: &RecordKey, payload: &[u8], revision: Revision) -> Record {
    Record {
        key: key.clone(),
        kind: RecordKind::Topic,
        revision,
        parent: None,
        body: Body::Inline(payload.to_vec()),
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

fn committed(result: PutResult) -> Revision {
    match result {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    }
}

#[test]
fn with_ancestor_cap_rejects_zero() {
    assert!(
        FsStore::new("/nonexistent-gonzalo-root")
            .with_ancestor_cap(0)
            .is_err()
    );
    assert!(
        FsStore::new("/nonexistent-gonzalo-root")
            .with_ancestor_cap(1)
            .is_ok()
    );
}

/// Six writes to `key`, each conditional on the previous revision, through
/// `put_raw` when `raw` is true and consumer `put` otherwise. Returns every
/// committed revision, oldest first.
async fn write_chain(store: &FsStore, key: &RecordKey, raw: bool) -> Vec<Revision> {
    let mut history: Vec<Revision> = Vec::new();
    for i in 0..=5u32 {
        let body = format!("v{i}");
        let (revision, expected) = match history.last() {
            None => (Revision::initial(body.as_bytes()), None),
            Some(prev) => (prev.next(body.as_bytes()), Some(prev.clone())),
        };
        let record = rec(key, body.as_bytes(), revision.clone());
        let result = if raw {
            store.put_raw(record, expected).await.unwrap()
        } else {
            store.put(record, expected).await.unwrap()
        };
        let stored = committed(result);
        // Neither path re-stamps a write over a live record.
        assert_eq!(stored, revision);
        history.push(stored);
    }
    history
}

#[tokio::test]
async fn put_folds_ancestors_to_the_configured_cap() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path()).with_ancestor_cap(3).unwrap();
    let key = RecordKey::new("ns", "col", "capped");
    let history = write_chain(&store, &key, false).await;

    let raw = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(raw.revision, history[5]);
    // Newest first, own revision excluded, truncated to the cap of 3.
    assert_eq!(
        raw.ancestors,
        vec![history[4].clone(), history[3].clone(), history[2].clone()]
    );
}

#[tokio::test]
async fn put_raw_folds_ancestors_to_the_configured_cap() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path()).with_ancestor_cap(3).unwrap();
    let key = RecordKey::new("ns", "col", "capped-raw");
    let history = write_chain(&store, &key, true).await;

    let raw = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(raw.revision, history[5]);
    assert_eq!(
        raw.ancestors,
        vec![history[4].clone(), history[3].clone(), history[2].clone()]
    );
}

#[tokio::test]
async fn purge_removes_the_record_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "gone");
    let rev = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    let path = dir.path().join("ns").join("col").join("gone.json");
    assert!(path.exists());

    assert_eq!(store.purge(&key, rev).await.unwrap(), DeleteResult::Deleted);
    assert!(!path.exists(), "purge must unlink the record file");
    assert_eq!(store.get_raw(&key).await.unwrap(), None);
    assert!(
        !store
            .list_raw(&KeyPrefix::default())
            .await
            .unwrap()
            .contains(&key)
    );
}

#[tokio::test]
async fn purge_with_stale_expected_conflicts_and_keeps_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "kept");
    let rev = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    let wrong = Revision::initial(b"never-current");

    match store.purge(&key, wrong).await.unwrap() {
        DeleteResult::Conflict(c) => {
            assert_eq!(c.key, key);
            assert_eq!(c.current.revision, rev);
        }
        DeleteResult::Deleted => panic!("stale purge must conflict"),
    }
    assert!(dir.path().join("ns").join("col").join("kept.json").exists());
}

#[tokio::test]
async fn purge_of_absent_key_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "never");
    assert_eq!(
        store
            .purge(&key, Revision::initial(b"anything"))
            .await
            .unwrap(),
        DeleteResult::Deleted
    );
}
