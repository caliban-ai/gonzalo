//! The ticket's acceptance criterion: an index of real size is written, dropped,
//! reopened, and queried — with the reopen cost reported rather than assumed.

use gonzalo_core::{
    BlobStore as _, Body, CoreError, DeleteResult, KeyPrefix, MANIFEST_TOMBSTONE_RECREATE_REJECTED,
    PutResult, RecordKey, RecordKind, Store as _, SweepPolicy, VectorManifest, collect,
    gc_blobs_with, now_ms, undelete,
};
use gonzalo_store_fs::FsStore;
use gonzalo_vector::{RecordVectorIndex, VectorIndex as _};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

const N: usize = 10_000;
const DIM: usize = 16;

#[tokio::test]
async fn ten_thousand_vectors_survive_a_reopen() {
    let dir = TempDir::new().unwrap();
    let store = FsStore::new(dir.path());
    let key = VectorManifest::key("ns", "acceptance");

    let items: Vec<(RecordKey, Vec<f32>)> = (0..N)
        .map(|i| {
            let mut v = vec![0.0; DIM];
            v[i % DIM] = 1.0;
            (RecordKey::new("ns", "coll", i.to_string()), v)
        })
        .collect();

    let probe = items[0].1.clone();

    let idx = RecordVectorIndex::open(
        FsStore::new(dir.path()),
        key.clone(),
        "acceptance-space",
        DIM,
    )
    .await
    .unwrap();
    idx.upsert_many(items).await.unwrap();
    drop(idx);

    let started = std::time::Instant::now();
    let reopened = RecordVectorIndex::open(store, key, "acceptance-space", DIM)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(reopened.keys(&KeyPrefix::default()).await.unwrap().len(), N);
    let hits = reopened
        .query(&probe, 5, &KeyPrefix::default())
        .await
        .unwrap();
    assert_eq!(hits.len(), 5);

    println!("reopened {N} vectors (dim {DIM}) in {elapsed:?}");
}

/// A deleted vector index is recoverable for the tombstone's lifetime: the
/// shards survive a sweep taken while it is deleted, and after `undelete` the
/// index opens and answers queries (ADR 0029).
#[tokio::test]
async fn a_deleted_index_can_be_undeleted_and_queried() {
    let dir = TempDir::new().unwrap();
    let store = FsStore::new(dir.path());
    let key = VectorManifest::key("ns", "restored");

    let mut probe = vec![0.0; DIM];
    probe[3] = 1.0;

    let idx = RecordVectorIndex::open(FsStore::new(dir.path()), key.clone(), "restored-space", DIM)
        .await
        .unwrap();
    idx.upsert_many(vec![(RecordKey::new("ns", "coll", "a"), probe.clone())])
        .await
        .unwrap();
    drop(idx);

    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );

    // A sweep with no grace period, while the index is deleted. The tombstone's
    // retained body is the only thing keeping these shards alive.
    let swept = gc_blobs_with(
        &store,
        SweepPolicy {
            min_age: Duration::ZERO,
            now: SystemTime::now(),
        },
    )
    .await
    .unwrap();
    assert!(swept.freed.is_empty(), "the shards were pinned");

    undelete(&store, &key, now_ms(), None).await.unwrap();

    let reopened = RecordVectorIndex::open(FsStore::new(dir.path()), key, "restored-space", DIM)
        .await
        .unwrap();
    assert_eq!(reopened.keys(&KeyPrefix::default()).await.unwrap().len(), 1);
    let hits = reopened
        .query(&probe, 1, &KeyPrefix::default())
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "the restored index answers queries");
}

/// The hazard #333 exists for: reopening a deleted index and writing to it used
/// to overwrite the tombstone and orphan the shards. #333 made the commit refuse;
/// gonzalo#340 moves the refusal forward to `open`, so the call that was actually
/// wrong is the one that fails. Either way the shards stay pinned (ADR 0030).
#[tokio::test]
async fn reopening_a_deleted_index_is_refused_and_the_shards_stay_pinned() {
    let dir = TempDir::new().unwrap();
    let store = FsStore::new(dir.path());
    let key = VectorManifest::key("ns", "guarded");

    let mut probe = vec![0.0; DIM];
    probe[1] = 1.0;

    let idx = RecordVectorIndex::open(FsStore::new(dir.path()), key.clone(), "guarded-space", DIM)
        .await
        .unwrap();
    idx.upsert_many(vec![(RecordKey::new("ns", "coll", "a"), probe.clone())])
        .await
        .unwrap();
    drop(idx);

    let _ = store.delete(&key, None).await.unwrap();
    // The guard keys on this field of the stored tombstone.
    let tomb = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(tomb.deleted_kind, Some(RecordKind::VectorManifest));

    // The shards the tombstone pins, read from its retained manifest body. The
    // refused commit below stages a blob of its own that nothing will ever
    // reference, so the sweep frees exactly that one; these must survive.
    let pinned: Vec<_> = VectorManifest::from_body(&tomb.body)
        .unwrap()
        .entries
        .into_values()
        .collect();
    assert!(!pinned.is_empty(), "the tombstone retains shard hashes");

    // Reopen: `open` itself refuses now (gonzalo#340), naming the key and the
    // remedy, instead of handing back a handle that fails at the first commit.
    let err = RecordVectorIndex::open(FsStore::new(dir.path()), key.clone(), "guarded-space", DIM)
        .await
        .expect_err("open must refuse while a tombstone holds the key");
    assert!(
        matches!(&err, CoreError::Invalid(m)
            if m.contains(MANIFEST_TOMBSTONE_RECREATE_REJECTED) && m.contains(&key.to_string())),
        "the refusal must name the key and the remedy; got {err:?}"
    );

    // The window survived, so the shards are still pinned. Refusing at `open`
    // also means no doomed commit staged a shard blob, so there is nothing for
    // the sweep to reclaim at all — where the commit-time refusal left exactly
    // one orphan behind.
    let swept = gc_blobs_with(
        &store,
        SweepPolicy {
            min_age: Duration::ZERO,
            now: SystemTime::now(),
        },
    )
    .await
    .unwrap();
    assert!(
        swept.freed.is_empty(),
        "a refusal at open stages no orphan shard; freed {:?}",
        swept.freed
    );
    for hash in &pinned {
        assert!(
            !swept.freed.contains(hash),
            "the sweep freed a pinned shard"
        );
        assert!(
            store.has_blob(hash).await.unwrap(),
            "a pinned shard is gone"
        );
    }

    // And the record is still restorable.
    undelete(&store, &key, now_ms(), None).await.unwrap();
    let restored = RecordVectorIndex::open(FsStore::new(dir.path()), key, "guarded-space", DIM)
        .await
        .unwrap();
    assert_eq!(restored.keys(&KeyPrefix::default()).await.unwrap().len(), 1);
}

/// Review Focus 2: the guard must not brick the key. Once `collect` has removed
/// the tombstone there is no window left to protect, so a create must work
/// again — otherwise a deleted index's key becomes permanently unwritable.
#[tokio::test]
async fn after_collect_removes_the_tombstone_a_fresh_index_can_be_created() {
    let dir = TempDir::new().unwrap();
    let store = FsStore::new(dir.path());
    let key = VectorManifest::key("ns", "recycled");

    let mut probe = vec![0.0; DIM];
    probe[2] = 1.0;

    let idx = RecordVectorIndex::open(FsStore::new(dir.path()), key.clone(), "recycled-space", DIM)
        .await
        .unwrap();
    idx.upsert_many(vec![(RecordKey::new("ns", "coll", "a"), probe.clone())])
        .await
        .unwrap();
    drop(idx);
    let _ = store.delete(&key, None).await.unwrap();
    // The guard keys on this field of the stored tombstone.
    let tomb = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(tomb.deleted_kind, Some(RecordKind::VectorManifest));

    // Before the collect the window is still open, so `open` is refused
    // (gonzalo#340 — previously this succeeded and the upsert was refused).
    let err = RecordVectorIndex::open(FsStore::new(dir.path()), key.clone(), "recycled-space", DIM)
        .await
        .expect_err("the tombstone's window must still refuse an open");
    assert!(
        matches!(&err, CoreError::Invalid(m) if m.contains(MANIFEST_TOMBSTONE_RECREATE_REJECTED)),
        "got {err:?}"
    );

    // Past the horizon: collect removes the tombstone, and with it the window.
    let collected = collect(&store, &KeyPrefix::default(), Duration::ZERO, now_ms() + 1)
        .await
        .unwrap();
    assert_eq!(collected.purged.len(), 1);

    // A fresh index at the same key now commits normally.
    let fresh = RecordVectorIndex::open(FsStore::new(dir.path()), key, "recycled-space", DIM)
        .await
        .unwrap();
    fresh
        .upsert_many(vec![(RecordKey::new("ns", "coll", "b"), probe)])
        .await
        .expect("with no tombstone there is no window to protect");
    assert_eq!(fresh.keys(&KeyPrefix::default()).await.unwrap().len(), 1);
}

/// gonzalo#340's trigger must match `plan_put`'s, which keys on the tombstone's
/// `deleted_kind`. A tombstone written before gonzalo#327 carries `None` there
/// and retains no body, so there is no restore window to protect and
/// `plan_put` lets a create through. `open` must therefore let it through too —
/// refusing on *any* tombstone would strand such a key, unwritable, with
/// nothing to restore.
#[tokio::test]
async fn open_over_a_tombstone_with_no_restore_window_starts_fresh() {
    let dir = TempDir::new().unwrap();
    let store = FsStore::new(dir.path());
    let key = VectorManifest::key("ns", "legacy");

    let mut probe = vec![0.0; DIM];
    probe[3] = 1.0;

    let idx = RecordVectorIndex::open(FsStore::new(dir.path()), key.clone(), "legacy-space", DIM)
        .await
        .unwrap();
    idx.upsert_many(vec![(RecordKey::new("ns", "coll", "a"), probe.clone())])
        .await
        .unwrap();
    drop(idx);
    let _ = store.delete(&key, None).await.unwrap();

    // Rewrite the tombstone into the shape gonzalo#327 predates: no retained
    // body, no `deleted_kind`. `put_raw` stores it verbatim, which is the same
    // path replication uses.
    let tomb = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(tomb.deleted_kind, Some(RecordKind::VectorManifest));
    let legacy = gonzalo_core::Record {
        body: Body::Inline(Vec::new()),
        deleted_kind: None,
        ..tomb.clone()
    };
    assert!(matches!(
        store
            .put_raw(legacy, Some(tomb.revision.clone()))
            .await
            .unwrap(),
        PutResult::Committed(_)
    ));
    let rewritten = store.get_raw(&key).await.unwrap().unwrap();
    assert!(rewritten.is_tombstone(), "still a tombstone");
    assert_eq!(rewritten.deleted_kind, None, "no restore window");

    // `open` starts fresh rather than refusing...
    let fresh = RecordVectorIndex::open(FsStore::new(dir.path()), key.clone(), "legacy-space", DIM)
        .await
        .expect("a tombstone with no restore window must not refuse open");
    assert!(
        fresh.keys(&KeyPrefix::default()).await.unwrap().is_empty(),
        "a fresh index starts empty"
    );

    // ...and the first commit goes out as a create, which `plan_put` accepts.
    // This is what pins `last_seen` staying `None`: naming the tombstone's
    // revision instead would make `plan_put` answer `NotFound`.
    fresh
        .upsert_many(vec![(RecordKey::new("ns", "coll", "b"), probe)])
        .await
        .expect("the first commit must land as a create");
    assert_eq!(fresh.keys(&KeyPrefix::default()).await.unwrap().len(), 1);
}
