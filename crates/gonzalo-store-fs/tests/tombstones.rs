//! fs-specific tombstone behaviour (gonzalo#203, spec §3.3): where tombstones
//! live on disk, how `purge` unlinks them, and how the ancestor cap is wired.
//! The substrate-independent semantics are covered by
//! `run_tombstone_conformance` in `tests/conformance.rs`.

use gonzalo_core::{
    Body, CoreError, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey,
    RecordKind, Revision, Store, tombstone_hash,
};
use gonzalo_store_fs::FsStore;
use std::collections::BTreeMap;
use std::sync::Arc;

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
        deleted_blob: None,
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

#[tokio::test]
async fn delete_writes_a_tombstone_at_the_record_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "doomed");
    let rev0 = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );

    assert_eq!(
        store
            .delete_as(&key, Some(rev0.clone()), Some(Identity::new("deleter")))
            .await
            .unwrap(),
        DeleteResult::Deleted
    );

    let path = dir.path().join("ns").join("col").join("doomed.json");
    let bytes = std::fs::read(&path).expect("the tombstone stays at the record path");
    let on_disk: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(on_disk["kind"], "Tombstone");
    assert!(on_disk["deleted_at"].is_i64());

    let tomb: Record = serde_json::from_slice(&bytes).unwrap();
    assert!(tomb.is_tombstone());
    assert_eq!(
        tomb.revision,
        Revision {
            counter: rev0.counter + 1,
            hash: tombstone_hash(),
        }
    );
    assert_eq!(tomb.parent, Some(rev0.clone()));
    assert_eq!(tomb.ancestors, vec![rev0]);
    // `delete_as` stamps the deleting principal onto the tombstone.
    assert_eq!(tomb.meta.author, Identity::new("deleter"));
    // The atomic temp+rename publish leaves no temp file behind.
    assert!(!path.with_extension("json.tmp").exists());
}

/// A no-op delete (over an existing tombstone) must leave the stored
/// tombstone byte-identical, not just revision-equal: a store that rewrote
/// `deleted_at` at the same revision would still pass a revision-only check.
#[tokio::test]
async fn second_delete_leaves_the_tombstone_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "twice-doomed");
    committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );

    let path = dir.path().join("ns").join("col").join("twice-doomed.json");
    let bytes_before = std::fs::read(&path).unwrap();
    let raw_before = store.get_raw(&key).await.unwrap().unwrap();

    // Long enough that a rewritten `deleted_at` (wall-clock ms) would differ.
    std::thread::sleep(std::time::Duration::from_millis(5));

    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );

    let bytes_after = std::fs::read(&path).unwrap();
    assert_eq!(
        bytes_after, bytes_before,
        "a no-op delete must not rewrite the stored tombstone"
    );
    let raw_after = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(raw_after, raw_before);
}

#[tokio::test]
async fn consumer_reads_hide_a_tombstone_and_raw_reads_show_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let dead = RecordKey::new("ns", "col", "dead");
    let live = RecordKey::new("ns", "col", "live");
    committed(
        store
            .put(rec(&dead, b"d", Revision::initial(b"d")), None)
            .await
            .unwrap(),
    );
    committed(
        store
            .put(rec(&live, b"l", Revision::initial(b"l")), None)
            .await
            .unwrap(),
    );
    assert_eq!(
        store.delete(&dead, None).await.unwrap(),
        DeleteResult::Deleted
    );

    assert_eq!(store.get(&dead).await.unwrap(), None);
    assert!(store.get_raw(&dead).await.unwrap().unwrap().is_tombstone());

    let all = KeyPrefix::default();
    assert_eq!(store.list(&all).await.unwrap(), vec![live.clone()]);
    let raw = store.list_raw(&all).await.unwrap();
    assert_eq!(raw.len(), 2);
    assert!(raw.contains(&dead) && raw.contains(&live));
}

#[tokio::test]
async fn purge_removes_a_tombstone_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "collected");
    committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );
    let tomb = store.get_raw(&key).await.unwrap().unwrap();
    let path = dir.path().join("ns").join("col").join("collected.json");
    assert!(path.exists());

    assert_eq!(
        store.purge(&key, tomb.revision).await.unwrap(),
        DeleteResult::Deleted
    );
    assert!(!path.exists());
    assert_eq!(store.get_raw(&key).await.unwrap(), None);
}

/// Consumer `list` must read each file to filter tombstones, but a `*.json`
/// that doesn't parse as a `Record` stays listed exactly as it was before
/// tombstones, and `get` keeps surfacing the parse error loudly.
#[tokio::test]
async fn list_keeps_an_unparseable_record_file_listed() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let good = RecordKey::new("ns", "col", "good");
    committed(
        store
            .put(rec(&good, b"g", Revision::initial(b"g")), None)
            .await
            .unwrap(),
    );
    std::fs::write(
        dir.path().join("ns").join("col").join("garbage.json"),
        b"not json",
    )
    .unwrap();
    let garbage = RecordKey::new("ns", "col", "garbage");

    let listed = store.list(&KeyPrefix::default()).await.unwrap();
    assert!(listed.contains(&good));
    assert!(listed.contains(&garbage));
    assert!(matches!(
        store.get(&garbage).await,
        Err(CoreError::Serde(_))
    ));
}

/// A `*.json` entry that isn't even a readable file (e.g. a stray directory)
/// keeps today's listing behaviour: consumer `list` still surfaces the key,
/// leaving `get` to report the read error instead of `list` failing outright.
#[tokio::test]
async fn list_keeps_an_unreadable_json_entry_listed() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let good = RecordKey::new("ns", "col", "good");
    committed(
        store
            .put(rec(&good, b"g", Revision::initial(b"g")), None)
            .await
            .unwrap(),
    );
    // A directory named `<id>.json` sitting next to the real record: it
    // matches the `*.json` walk in `collect_keys` but can't be read as a file.
    std::fs::create_dir(dir.path().join("ns").join("col").join("stray.json")).unwrap();
    let stray = RecordKey::new("ns", "col", "stray");

    let listed = store.list(&KeyPrefix::default()).await.unwrap();
    assert!(listed.contains(&good));
    assert!(listed.contains(&stray));
    // The rationale for keeping it listed depends on `get` actually
    // surfacing the read error rather than silently treating it as absent.
    assert!(store.get(&stray).await.is_err());
}

/// Deletes go through the per-record lock: N racing deletes of one revision
/// all report `Deleted` with no errors. Exactly one tombstone is written; the
/// rest see it and no-op.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_deletes_of_one_revision_write_one_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsStore::new(dir.path()));
    let key = RecordKey::new("ns", "col", "raced");
    let base = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );

    let mut handles = Vec::new();
    for _ in 0..16 {
        let store = Arc::clone(&store);
        let key = key.clone();
        let expected = base.clone();
        handles.push(tokio::spawn(async move {
            store.delete(&key, Some(expected)).await
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap().unwrap(), DeleteResult::Deleted);
    }

    let tomb = store.get_raw(&key).await.unwrap().unwrap();
    assert!(tomb.is_tombstone());
    assert_eq!(tomb.revision.counter, base.counter + 1);
    assert_eq!(tomb.parent, Some(base));
}

/// Consumer `put` treats a tombstoned key as absent: a conditional write
/// naming any revision (even the tombstone's own) is `NotFound`, and an
/// unconditional write is a re-stamped recreation.
#[tokio::test]
async fn consumer_put_over_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "reborn");
    committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );
    let tomb = store.get_raw(&key).await.unwrap().unwrap();

    assert!(matches!(
        store
            .put(
                rec(&key, b"y", Revision::initial(b"y")),
                Some(tomb.revision.clone())
            )
            .await,
        Err(CoreError::NotFound(_))
    ));

    let recreated = committed(
        store
            .put(rec(&key, b"y", Revision::initial(b"y")), None)
            .await
            .unwrap(),
    );
    assert_eq!(recreated.counter, tomb.revision.counter + 1);
    let stored = store.get(&key).await.unwrap().unwrap();
    assert_eq!(stored.parent, Some(tomb.revision));
}

/// Replication `put_raw` never re-stamps: a create over a tombstone conflicts
/// with the tombstone as `current`, and a write conditional on the
/// tombstone's revision stores the caller's revision verbatim.
#[tokio::test]
async fn put_raw_over_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "replicated");
    committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );
    let tomb = store.get_raw(&key).await.unwrap().unwrap();

    match store
        .put_raw(rec(&key, b"y", Revision::initial(b"y")), None)
        .await
        .unwrap()
    {
        PutResult::Conflict(c) => assert!(c.current.is_tombstone()),
        PutResult::Committed(rev) => panic!("create over a tombstone must conflict, got {rev:?}"),
    }
    // The conflicting create wrote nothing.
    assert_eq!(store.get_raw(&key).await.unwrap().unwrap(), tomb);

    let incoming = Revision {
        counter: 7,
        hash: gonzalo_core::ContentHash::of(b"peer"),
    };
    assert_eq!(
        committed(
            store
                .put_raw(
                    rec(&key, b"peer", incoming.clone()),
                    Some(tomb.revision.clone())
                )
                .await
                .unwrap()
        ),
        incoming
    );
    let stored = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(stored.revision, incoming);
    assert!(stored.ancestors.contains(&tomb.revision));
}
