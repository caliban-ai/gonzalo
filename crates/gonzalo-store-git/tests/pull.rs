//! Non-fast-forward `pull` performs a content-aware 3-way merge over real
//! diverged git repos (#7 / ADR 0017).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gonzalo_core::{
    Body, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind,
    Revision, Store,
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
        ancestors: Vec::new(),
        deleted_at: None,
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

fn body_of(store_get: Option<Record>) -> serde_json::Value {
    serde_json::from_slice(store_get.unwrap().body.bytes()).unwrap()
}

/// A remote repo with a base record `m`, cloned to a local repo. Returns
/// (`_remote_dir`, `_local_dir`, remote store, local store, local repo path,
/// branch, base revision) — the tempdir guards must stay live.
async fn cloned_base(
    kind: RecordKind,
    base_json: &str,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    GitStore,
    GitStore,
    PathBuf,
    String,
    Revision,
) {
    let remote_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();
    let remote = GitStore::open(remote_dir.path()).unwrap();
    let base_rev = Revision::initial(base_json.as_bytes());
    commit(
        &remote,
        record("m", kind, base_json, base_rev.clone(), None),
        None,
    )
    .await;

    let local_path = local_dir.path().join("clone");
    git2::Repository::clone(remote_dir.path().to_str().unwrap(), &local_path).unwrap();
    let local = GitStore::open(&local_path).unwrap();
    let branch = branch_of(&local_path);
    (
        remote_dir, local_dir, remote, local, local_path, branch, base_rev,
    )
}

#[tokio::test]
async fn nonff_pull_merges_disjoint_structured_edits() {
    let (_r, _l, remote, local, local_path, branch, base_rev) =
        cloned_base(RecordKind::MemoryTier, r#"{"name":"a","content":"x"}"#).await;

    // Remote changes `content`; local changes `name` — disjoint fields.
    commit(
        &remote,
        record(
            "m",
            RecordKind::MemoryTier,
            r#"{"name":"a","content":"y"}"#,
            base_rev.next(b"remote"),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;
    commit(
        &local,
        record(
            "m",
            RecordKind::MemoryTier,
            r#"{"name":"b","content":"x"}"#,
            base_rev.next(b"local"),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;

    let report = local.pull("origin", &branch).await.unwrap();
    assert!(!report.fast_forwarded);
    assert_eq!(report.merged, vec![key("m")]);
    assert!(report.conflicts.is_empty());

    // Both field edits applied against the real (merge-base) ancestor.
    assert_eq!(
        body_of(local.get(&key("m")).await.unwrap()),
        serde_json::json!({"name": "b", "content": "y"})
    );

    // The merge commit has two parents.
    let repo = git2::Repository::open(&local_path).unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.parent_count(), 2);
}

#[tokio::test]
async fn nonff_pull_surfaces_unmergeable_conflict_and_keeps_local() {
    // Checkpoint is Opaque → a both-sided change never auto-merges.
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Checkpoint, "base").await;
    commit(
        &remote,
        record(
            "m",
            RecordKind::Checkpoint,
            "remote",
            base_rev.next(b"remote"),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;
    commit(
        &local,
        record(
            "m",
            RecordKind::Checkpoint,
            "local",
            base_rev.next(b"local"),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;

    let report = local.pull("origin", &branch).await.unwrap();
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].key, key("m"));
    assert!(report.merged.is_empty());
    // Local is retained; the remote side is surfaced.
    assert_eq!(
        local.get(&key("m")).await.unwrap().unwrap().body.bytes(),
        b"local"
    );
    assert_eq!(report.conflicts[0].remote.body.bytes(), b"remote");
}

#[tokio::test]
async fn nonff_pull_applies_one_sided_remote_change() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::MemoryTier, r#"{"v":0}"#).await;

    // Remote adds a new record `n`; local edits `m`. Diverged, but no record
    // changed on both sides.
    commit(
        &remote,
        record(
            "n",
            RecordKind::MemoryTier,
            r#"{"added":true}"#,
            Revision::initial(br#"{"added":true}"#),
            None,
        ),
        None,
    )
    .await;
    commit(
        &local,
        record(
            "m",
            RecordKind::MemoryTier,
            r#"{"v":1}"#,
            base_rev.next(b"local"),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;

    let report = local.pull("origin", &branch).await.unwrap();
    assert!(report.merged.is_empty() && report.conflicts.is_empty());
    // Remote's new record is present; local's edit to `m` is retained.
    assert_eq!(
        body_of(local.get(&key("n")).await.unwrap()),
        serde_json::json!({"added": true})
    );
    assert_eq!(
        body_of(local.get(&key("m")).await.unwrap()),
        serde_json::json!({"v": 1})
    );
}

#[tokio::test]
async fn pull_fast_forwards_when_only_remote_advanced() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::MemoryTier, r#"{"v":0}"#).await;
    // Only the remote advances; local is unchanged → fast-forward.
    commit(
        &remote,
        record(
            "m",
            RecordKind::MemoryTier,
            r#"{"v":9}"#,
            base_rev.next(b"remote"),
            Some(base_rev.clone()),
        ),
        Some(base_rev),
    )
    .await;

    let report = local.pull("origin", &branch).await.unwrap();
    assert!(report.fast_forwarded);
    assert!(report.merged.is_empty() && report.conflicts.is_empty());
    assert_eq!(
        body_of(local.get(&key("m")).await.unwrap()),
        serde_json::json!({"v": 9})
    );
}

#[tokio::test]
async fn pull_up_to_date_is_a_noop() {
    let (_r, _l, _remote, local, _p, branch, _base) =
        cloned_base(RecordKind::MemoryTier, r#"{"v":0}"#).await;
    let report = local.pull("origin", &branch).await.unwrap();
    assert!(!report.fast_forwarded);
    assert!(report.merged.is_empty() && report.conflicts.is_empty());
}

/// Delete `id` expecting `expected`, and return the tombstone it wrote.
async fn tombstone(store: &GitStore, id: &str, expected: Revision) -> Record {
    assert!(matches!(
        store.delete(&key(id), Some(expected)).await.unwrap(),
        DeleteResult::Deleted
    ));
    let t = store.get_raw(&key(id)).await.unwrap().unwrap();
    assert!(t.is_tombstone());
    t
}

/// Commit an unrelated new record `n`. Used to make a side diverge without
/// touching `m`.
async fn add_n(store: &GitStore) {
    commit(
        store,
        record(
            "n",
            RecordKind::MemoryTier,
            r#"{"added":true}"#,
            Revision::initial(br#"{"added":true}"#),
            None,
        ),
        None,
    )
    .await;
}

#[tokio::test]
async fn pull_fast_forwards_tombstone() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let t = tombstone(&remote, "m", base_rev).await;

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(report.fast_forwarded);
    assert!(local.get(&key("m")).await.unwrap().is_none());
    assert_eq!(
        local.get_raw(&key("m")).await.unwrap().unwrap().revision,
        t.revision
    );
    assert!(
        !local
            .list(&KeyPrefix::default())
            .await
            .unwrap()
            .contains(&key("m"))
    );
}

#[tokio::test]
async fn nonff_pull_applies_remote_tombstone() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let t = tombstone(&remote, "m", base_rev).await;
    add_n(&local).await; // diverge without touching `m`

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(!report.fast_forwarded);
    assert!(report.merged.is_empty() && report.conflicts.is_empty());
    assert!(local.get(&key("m")).await.unwrap().is_none());
    assert_eq!(
        local.get_raw(&key("m")).await.unwrap().unwrap().revision,
        t.revision
    );
    assert!(local.get(&key("n")).await.unwrap().is_some());
}

#[tokio::test]
async fn nonff_pull_keeps_local_tombstone() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let t = tombstone(&local, "m", base_rev).await;
    add_n(&remote).await;

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(!report.fast_forwarded);
    assert!(report.merged.is_empty() && report.conflicts.is_empty());
    assert!(local.get(&key("m")).await.unwrap().is_none());
    assert_eq!(
        local.get_raw(&key("m")).await.unwrap().unwrap().revision,
        t.revision
    );
    assert!(local.get(&key("n")).await.unwrap().is_some());
}

#[tokio::test]
async fn nonff_pull_surfaces_delete_vs_edit_conflict() {
    // Topic is AppendOnly, and local is the live side, so without the
    // tombstone check `merge(local.kind.merge_class(), ..)` would "merge"
    // away the remote delete.
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let _ = tombstone(&remote, "m", base_rev.clone()).await;
    commit(
        &local,
        record(
            "m",
            RecordKind::Topic,
            "base\nlocal\n",
            base_rev.next(b"base\nlocal\n"),
            Some(base_rev.clone()),
        ),
        Some(base_rev),
    )
    .await;

    let report = local.pull("origin", &branch).await.unwrap();

    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].key, key("m"));
    assert!(report.conflicts[0].remote.is_tombstone());
    assert!(!report.conflicts[0].local.is_tombstone());
    assert!(report.merged.is_empty());
    // Local is kept.
    assert_eq!(
        local.get(&key("m")).await.unwrap().unwrap().body.bytes(),
        b"base\nlocal\n"
    );
}

#[tokio::test]
async fn pull_fast_forwards_purge() {
    let (_r, _l, remote, local, local_path, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let t = tombstone(&remote, "m", base_rev).await;
    assert!(matches!(
        remote.purge(&key("m"), t.revision).await.unwrap(),
        DeleteResult::Deleted
    ));

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(report.fast_forwarded);
    assert!(
        local.get_raw(&key("m")).await.unwrap().is_none(),
        "ff pull left the purged file in the worktree"
    );
    assert!(
        !local
            .list_raw(&KeyPrefix::default())
            .await
            .unwrap()
            .contains(&key("m"))
    );
    assert!(!local_path.join("ns/col/m.json").exists());
}

#[tokio::test]
async fn nonff_pull_surfaces_edit_vs_local_delete_conflict() {
    // Mirror of `nonff_pull_surfaces_delete_vs_edit_conflict`: the tombstone is
    // local and the edit is remote. Topic is AppendOnly, so without the
    // tombstone check this would reach the body merge and resurrect the record.
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let remote_rev = base_rev.next(b"base\nremote\n");
    commit(
        &remote,
        record(
            "m",
            RecordKind::Topic,
            "base\nremote\n",
            remote_rev.clone(),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;
    let tl = tombstone(&local, "m", base_rev).await;

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(!report.fast_forwarded);
    assert_eq!(report.conflicts.len(), 1);
    assert!(report.conflicts[0].local.is_tombstone());
    assert!(!report.conflicts[0].remote.is_tombstone());
    assert!(report.merged.is_empty());
    let got = local.get_raw(&key("m")).await.unwrap().unwrap();
    assert!(got.is_tombstone(), "local tombstone kept, not resurrected");
    assert_eq!(got.revision, tl.revision);
}

#[tokio::test]
async fn nonff_pull_converges_concurrent_tombstones() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    // Remote edits then deletes (tombstone counter 2); local deletes base (1).
    let r1 = base_rev.next(b"base\nr1\n");
    commit(
        &remote,
        record(
            "m",
            RecordKind::Topic,
            "base\nr1\n",
            r1.clone(),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;
    let tr = tombstone(&remote, "m", r1.clone()).await;
    let tl = tombstone(&local, "m", base_rev.clone()).await;
    assert_eq!(tr.revision.counter, 2);
    assert_eq!(tl.revision.counter, 1);

    let report = local.pull("origin", &branch).await.unwrap();

    assert_eq!(report.merged, vec![key("m")]);
    assert!(report.conflicts.is_empty());
    let got = local.get_raw(&key("m")).await.unwrap().unwrap();
    assert!(got.is_tombstone());
    assert_eq!(got.revision, tr.revision);
    assert!(got.ancestors.contains(&tl.revision));
    assert!(got.ancestors.contains(&r1));
    assert!(got.ancestors.contains(&base_rev));
    assert!(!got.ancestors.contains(&tr.revision));
}

#[tokio::test]
async fn nonff_pull_identical_tombstones_are_a_noop() {
    // Both sides delete the same revision: equal revisions, but the files
    // differ (deleted_at), so git sees a both-sided change.
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let tr = tombstone(&remote, "m", base_rev.clone()).await;
    let tl = tombstone(&local, "m", base_rev).await;
    assert_eq!(tr.revision, tl.revision);

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(report.merged.is_empty() && report.conflicts.is_empty());
    assert_eq!(
        local.get_raw(&key("m")).await.unwrap().unwrap().revision,
        tl.revision
    );
}

#[tokio::test]
async fn nonff_pull_applies_remote_purge() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let t = tombstone(&remote, "m", base_rev).await;
    assert!(matches!(
        remote.purge(&key("m"), t.revision).await.unwrap(),
        DeleteResult::Deleted
    ));
    add_n(&local).await;

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(!report.fast_forwarded);
    assert!(report.merged.is_empty() && report.conflicts.is_empty());
    assert!(local.get_raw(&key("m")).await.unwrap().is_none());
    assert!(
        !local
            .list_raw(&KeyPrefix::default())
            .await
            .unwrap()
            .contains(&key("m"))
    );
    assert!(local.get(&key("n")).await.unwrap().is_some());
}

#[tokio::test]
async fn nonff_pull_merged_record_folds_both_ancestries() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::MemoryTier, r#"{"name":"a","content":"x"}"#).await;
    let remote_rev = base_rev.next(b"remote");
    let local_rev = base_rev.next(b"local");
    commit(
        &remote,
        record(
            "m",
            RecordKind::MemoryTier,
            r#"{"name":"a","content":"y"}"#,
            remote_rev.clone(),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;
    commit(
        &local,
        record(
            "m",
            RecordKind::MemoryTier,
            r#"{"name":"b","content":"x"}"#,
            local_rev.clone(),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;

    let report = local.pull("origin", &branch).await.unwrap();

    assert_eq!(report.merged, vec![key("m")]);
    let merged = local.get_raw(&key("m")).await.unwrap().unwrap();
    assert!(merged.ancestors.contains(&local_rev));
    assert!(merged.ancestors.contains(&remote_rev));
    assert!(merged.ancestors.contains(&base_rev));
    assert!(!merged.ancestors.contains(&merged.revision));
}

#[tokio::test]
async fn nonff_pull_takes_remote_edit_over_local_purge() {
    // Both sides change `m` against the merge base: the remote edits it, and
    // local deletes then purges it (a git deletion). Pull must not silently
    // drop the concurrent remote edit; it takes the remote record, matching
    // sync's `(None, Some)` copy row (spec §3.5).
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let remote_rev = base_rev.next(b"base\nremote\n");
    commit(
        &remote,
        record(
            "m",
            RecordKind::Topic,
            "base\nremote\n",
            remote_rev.clone(),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;
    let t = tombstone(&local, "m", base_rev).await;
    assert!(matches!(
        local.purge(&key("m"), t.revision).await.unwrap(),
        DeleteResult::Deleted
    ));
    assert!(local.get_raw(&key("m")).await.unwrap().is_none());

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(!report.fast_forwarded);
    assert!(report.merged.is_empty() && report.conflicts.is_empty());
    let got = local.get(&key("m")).await.unwrap().unwrap();
    assert_eq!(got.body.bytes(), b"base\nremote\n");
    assert_eq!(got.revision, remote_rev);
    let raw = local.get_raw(&key("m")).await.unwrap().unwrap();
    assert_eq!(raw.revision, remote_rev);
    assert!(!raw.is_tombstone());
}
