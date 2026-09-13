//! git-specific tombstone behaviour (gonzalo#203, spec §3.3): a delete commits
//! a tombstone file at the record path, a no-op delete makes no commit, and
//! `purge` commits the removal. The substrate-independent semantics are
//! covered by `run_tombstone_conformance` in `tests/conformance.rs`.

use std::collections::BTreeMap;
use std::path::Path;

use gonzalo_core::{
    Body, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind,
    Revision, Store,
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
