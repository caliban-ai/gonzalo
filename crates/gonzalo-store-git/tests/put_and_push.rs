//! Concurrency safety of `GitStore::put` (#134) and non-fast-forward push
//! rejection reporting (#135).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use gonzalo_core::{
    Body, Identity, Meta, PutResult, Record, RecordKey, RecordKind, Revision, Store,
};
use gonzalo_store_git::GitStore;

fn record(
    id: &str,
    kind: RecordKind,
    json: &str,
    revision: Revision,
    parent: Option<Revision>,
) -> Record {
    Record {
        revision,
        parent,
        body: Body::Inline(json.as_bytes().to_vec()),
        kind,
        key: RecordKey::new("ns", "col", id),
        meta: Meta {
            author: Identity::new("t"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
    }
}

async fn commit(store: &GitStore, r: Record, expected: Option<Revision>) {
    assert!(matches!(
        store.put(r, expected).await.unwrap(),
        PutResult::Committed(_)
    ));
}

fn branch_of(path: &Path) -> String {
    git2::Repository::open(path)
        .unwrap()
        .head()
        .unwrap()
        .shorthand()
        .unwrap()
        .to_string()
}

fn key(id: &str) -> RecordKey {
    RecordKey::new("ns", "col", id)
}

/// #134: two concurrent conditional puts on the same key from the same store
/// must serialize — exactly one `Committed`, one `Conflict` — and the winner's
/// commit must not be orphaned: `get()` returns the winning revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_conditional_puts_yield_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(GitStore::open(dir.path()).unwrap());

    let a = record(
        "k",
        RecordKind::MemoryTier,
        r#"{"who":"a"}"#,
        Revision::initial(br#"{"who":"a"}"#),
        None,
    );
    let b = record(
        "k",
        RecordKind::MemoryTier,
        r#"{"who":"b"}"#,
        Revision::initial(br#"{"who":"b"}"#),
        None,
    );

    // Both race from the same precondition (key absent → expected = None).
    let s1 = store.clone();
    let s2 = store.clone();
    let h1 = tokio::spawn(async move { s1.put(a, None).await.unwrap() });
    let h2 = tokio::spawn(async move { s2.put(b, None).await.unwrap() });
    let r1 = h1.await.unwrap();
    let r2 = h2.await.unwrap();

    let committed = [&r1, &r2]
        .iter()
        .filter(|r| matches!(r, PutResult::Committed(_)))
        .count();
    let conflict = [&r1, &r2]
        .iter()
        .filter(|r| matches!(r, PutResult::Conflict(_)))
        .count();
    assert_eq!(committed, 1, "exactly one put must commit: {r1:?} {r2:?}");
    assert_eq!(conflict, 1, "the loser must see a Conflict: {r1:?} {r2:?}");

    // The winning commit is HEAD and its data is what `get` returns — no orphan.
    let winner_rev = match (&r1, &r2) {
        (PutResult::Committed(rev), _) | (_, PutResult::Committed(rev)) => rev.clone(),
        _ => unreachable!(),
    };
    let got = store.get(&key("k")).await.unwrap().unwrap();
    assert_eq!(
        got.revision, winner_rev,
        "get must return the committed winner"
    );
}

/// #135: a push the remote refuses as non-fast-forward must surface as `Err`,
/// not a silent `Ok` (libgit2 reports the rejection only via
/// `push_update_reference`).
#[tokio::test]
async fn push_reports_non_fast_forward_rejection_as_err() {
    let remote_dir = tempfile::tempdir().unwrap();
    git2::Repository::init_bare(remote_dir.path()).unwrap();
    let remote_url = remote_dir.path().to_str().unwrap();

    // Clone A, seed a base commit, and push it to establish the branch.
    let a_dir = tempfile::tempdir().unwrap();
    let a_path = a_dir.path().join("a");
    git2::Repository::clone(remote_url, &a_path).unwrap();
    let a = GitStore::open(&a_path).unwrap();
    let base_rev = Revision::initial(br#"{"v":0}"#);
    commit(
        &a,
        record(
            "m",
            RecordKind::MemoryTier,
            r#"{"v":0}"#,
            base_rev.clone(),
            None,
        ),
        None,
    )
    .await;
    let branch = branch_of(&a_path);
    a.push("origin", &branch).await.unwrap();

    // Clone B from the seeded remote so both share the base.
    let b_dir = tempfile::tempdir().unwrap();
    let b_path = b_dir.path().join("b");
    git2::Repository::clone(remote_url, &b_path).unwrap();
    let b = GitStore::open(&b_path).unwrap();

    // A advances the remote branch; this fast-forward must succeed.
    commit(
        &a,
        record(
            "m",
            RecordKind::MemoryTier,
            r#"{"v":1}"#,
            base_rev.next(b"a"),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;
    a.push("origin", &branch).await.unwrap();

    // B commits on the now-stale base and pushes — the remote must reject it as
    // non-fast-forward, and that rejection must be reported as an error.
    commit(
        &b,
        record(
            "m",
            RecordKind::MemoryTier,
            r#"{"v":2}"#,
            base_rev.next(b"b"),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;
    let res = b.push("origin", &branch).await;
    assert!(
        res.is_err(),
        "non-fast-forward push must be reported as Err, got {res:?}"
    );
}
