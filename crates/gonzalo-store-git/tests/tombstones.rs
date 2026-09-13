//! git-specific tombstone behaviour (gonzalo#203, spec §3.3): a delete commits
//! a tombstone file at the record path, a no-op delete makes no commit, and
//! `purge` commits the removal. The substrate-independent semantics are
//! covered by `run_tombstone_conformance` in `tests/conformance.rs`.

use std::collections::BTreeMap;
use std::path::Path;

use gonzalo_core::{
    Body, CoreError, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey,
    RecordKind, Revision, Store, tombstone_hash,
};
use gonzalo_store_git::GitStore;

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

/// `(HEAD commit id, HEAD commit message)` of the repo at `root`.
fn head_commit_of(root: &Path) -> (git2::Oid, String) {
    let repo = git2::Repository::open(root).unwrap();
    let commit = repo.head().unwrap().peel_to_commit().unwrap();
    (
        commit.id(),
        commit.message().unwrap_or_default().to_string(),
    )
}

/// The bytes committed at `rel` in HEAD's tree, or `None` if the path is absent.
fn head_tree_bytes(root: &Path, rel: &str) -> Option<Vec<u8>> {
    let repo = git2::Repository::open(root).unwrap();
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    let entry = tree.get_path(Path::new(rel)).ok()?;
    let object = entry.to_object(&repo).unwrap();
    Some(object.peel_to_blob().unwrap().content().to_vec())
}

#[test]
fn with_ancestor_cap_rejects_zero() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        GitStore::open(dir.path())
            .unwrap()
            .with_ancestor_cap(0)
            .is_err()
    );
    assert!(
        GitStore::open(dir.path())
            .unwrap()
            .with_ancestor_cap(1)
            .is_ok()
    );
}

/// Six writes to `key`, each conditional on the previous revision, through
/// `put_raw` when `raw` is true and consumer `put` otherwise. Returns every
/// committed revision, oldest first.
async fn write_chain(store: &GitStore, key: &RecordKey, raw: bool) -> Vec<Revision> {
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
async fn put_raw_folds_ancestors_to_the_configured_cap() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path())
        .unwrap()
        .with_ancestor_cap(3)
        .unwrap();
    let key = RecordKey::new("ns", "col", "capped-raw");
    let history = write_chain(&store, &key, true).await;

    let raw = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(raw.revision, history[5]);
    assert_eq!(
        raw.ancestors,
        vec![history[4].clone(), history[3].clone(), history[2].clone()]
    );
    let (_, message) = head_commit_of(dir.path());
    assert!(
        message.starts_with("put "),
        "got commit message {message:?}"
    );
}

#[tokio::test]
async fn put_folds_ancestors_to_the_configured_cap() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path())
        .unwrap()
        .with_ancestor_cap(3)
        .unwrap();
    let key = RecordKey::new("ns", "col", "capped");
    let history = write_chain(&store, &key, false).await;

    let raw = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(raw.revision, history[5]);
    assert_eq!(
        raw.ancestors,
        vec![history[4].clone(), history[3].clone(), history[2].clone()]
    );
    // The committed file carries the same folded ancestors as the worktree.
    let committed_rec: Record =
        serde_json::from_slice(&head_tree_bytes(dir.path(), "ns/col/capped.json").unwrap())
            .unwrap();
    assert_eq!(committed_rec.ancestors, raw.ancestors);
}

#[tokio::test]
async fn purge_commits_the_removal() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let key = RecordKey::new("ns", "col", "gone");
    let rev = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert!(head_tree_bytes(dir.path(), "ns/col/gone.json").is_some());

    assert_eq!(store.purge(&key, rev).await.unwrap(), DeleteResult::Deleted);

    assert!(
        head_tree_bytes(dir.path(), "ns/col/gone.json").is_none(),
        "purge must commit the removal"
    );
    assert!(!dir.path().join("ns/col/gone.json").exists());
    let (_, message) = head_commit_of(dir.path());
    assert!(
        message.starts_with("purge "),
        "got commit message {message:?}"
    );
    assert!(
        !store
            .list_raw(&KeyPrefix::default())
            .await
            .unwrap()
            .contains(&key)
    );
}

#[tokio::test]
async fn purge_with_stale_expected_conflicts_without_committing() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let key = RecordKey::new("ns", "col", "kept");
    let rev = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    let (before, _) = head_commit_of(dir.path());

    match store
        .purge(&key, Revision::initial(b"never-current"))
        .await
        .unwrap()
    {
        DeleteResult::Conflict(c) => {
            assert_eq!(c.key, key);
            assert_eq!(c.current.revision, rev);
        }
        DeleteResult::Deleted => panic!("stale purge must conflict"),
    }
    assert_eq!(head_commit_of(dir.path()).0, before);
    assert!(head_tree_bytes(dir.path(), "ns/col/kept.json").is_some());
}

#[tokio::test]
async fn delete_commits_a_tombstone_file_at_the_record_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
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

    let bytes = head_tree_bytes(dir.path(), "ns/col/doomed.json")
        .expect("the tombstone is committed at the record path");
    let on_disk: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(on_disk["kind"], "Tombstone");
    let tomb: Record = serde_json::from_slice(&bytes).unwrap();
    assert!(tomb.is_tombstone());
    assert_eq!(
        tomb.revision,
        Revision {
            counter: rev0.counter + 1,
            hash: tombstone_hash(),
        }
    );
    assert!(tomb.deleted_at.is_some());
    assert_eq!(tomb.ancestors, vec![rev0]);
    // `delete_as` stamps the deleting principal onto the committed tombstone.
    assert_eq!(tomb.meta.author, Identity::new("deleter"));

    let (_, message) = head_commit_of(dir.path());
    assert!(
        message.starts_with("delete "),
        "got commit message {message:?}"
    );
    // The worktree matches the commit.
    assert_eq!(
        std::fs::read(dir.path().join("ns/col/doomed.json")).unwrap(),
        bytes
    );
}

#[tokio::test]
async fn noop_deletes_make_no_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let key = RecordKey::new("ns", "col", "once");
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
    let (after_tombstone, _) = head_commit_of(dir.path());

    // Deleting a tombstone writes nothing.
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );
    // Deleting a key that never existed writes nothing.
    assert_eq!(
        store
            .delete(&RecordKey::new("ns", "col", "never"), None)
            .await
            .unwrap(),
        DeleteResult::Deleted
    );
    assert_eq!(head_commit_of(dir.path()).0, after_tombstone);
}

#[tokio::test]
async fn consumer_reads_hide_a_tombstone_and_raw_reads_show_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
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
async fn purge_removes_a_committed_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
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

    assert_eq!(
        store.purge(&key, tomb.revision).await.unwrap(),
        DeleteResult::Deleted
    );
    assert!(head_tree_bytes(dir.path(), "ns/col/collected.json").is_none());
    assert_eq!(store.get_raw(&key).await.unwrap(), None);
}

/// A `*.json` in the worktree that doesn't parse as a `Record` stays listed
/// by consumer `list`, exactly as before tombstones, and `get` surfaces the
/// parse error.
#[tokio::test]
async fn list_keeps_an_unparseable_record_file_listed() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let good = RecordKey::new("ns", "col", "good");
    committed(
        store
            .put(rec(&good, b"g", Revision::initial(b"g")), None)
            .await
            .unwrap(),
    );
    std::fs::write(dir.path().join("ns/col/garbage.json"), b"not json").unwrap();
    let garbage = RecordKey::new("ns", "col", "garbage");

    let listed = store.list(&KeyPrefix::default()).await.unwrap();
    assert!(listed.contains(&good));
    assert!(listed.contains(&garbage));
    assert!(matches!(
        store.get(&garbage).await,
        Err(CoreError::Serde(_))
    ));
}

/// A directory named `<id>.json` sitting next to a real committed record
/// matches the `*.json` walk in `collect_keys` but can't be read as a
/// record file. Consumer `list` keeps it listed, exactly as before
/// tombstones (Q2 ruling), rather than failing the whole listing.
#[tokio::test]
async fn list_keeps_an_unreadable_json_entry_listed() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let good = RecordKey::new("ns", "col", "good");
    committed(
        store
            .put(rec(&good, b"g", Revision::initial(b"g")), None)
            .await
            .unwrap(),
    );
    std::fs::create_dir(dir.path().join("ns").join("col").join("stray.json")).unwrap();
    let stray = RecordKey::new("ns", "col", "stray");

    let listed = store.list(&KeyPrefix::default()).await.unwrap();
    assert!(listed.contains(&good));
    assert!(listed.contains(&stray));
}

/// Consumer `put` treats a tombstoned key as absent: a conditional write
/// naming any revision (even the tombstone's own) is `NotFound` and commits
/// nothing; an unconditional write is a re-stamped recreation.
#[tokio::test]
async fn consumer_put_over_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
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
    let (after_tombstone, _) = head_commit_of(dir.path());

    assert!(matches!(
        store
            .put(
                rec(&key, b"y", Revision::initial(b"y")),
                Some(tomb.revision.clone())
            )
            .await,
        Err(CoreError::NotFound(_))
    ));
    assert_eq!(head_commit_of(dir.path()).0, after_tombstone);

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
/// with the tombstone as `current` and commits nothing; a write conditional on
/// the tombstone's revision commits the caller's revision verbatim.
#[tokio::test]
async fn put_raw_over_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
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
    let (after_tombstone, _) = head_commit_of(dir.path());

    match store
        .put_raw(rec(&key, b"y", Revision::initial(b"y")), None)
        .await
        .unwrap()
    {
        PutResult::Conflict(c) => assert!(c.current.is_tombstone()),
        PutResult::Committed(rev) => panic!("create over a tombstone must conflict, got {rev:?}"),
    }
    assert_eq!(head_commit_of(dir.path()).0, after_tombstone);

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
    let committed_rec: Record =
        serde_json::from_slice(&head_tree_bytes(dir.path(), "ns/col/replicated.json").unwrap())
            .unwrap();
    assert_eq!(committed_rec.revision, incoming);
    assert!(committed_rec.ancestors.contains(&tomb.revision));
}
