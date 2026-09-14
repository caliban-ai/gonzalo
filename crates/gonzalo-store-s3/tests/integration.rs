use gonzalo_core::conformance::{run_store_conformance, run_tombstone_conformance};
use gonzalo_core::{
    BlobStore, Body, ContentHash, CoreError, DEFAULT_ANCESTOR_CAP, DeleteResult, Identity, Meta,
    PutResult, Record, RecordKey, RecordKind, Revision, Store,
};
use gonzalo_store_s3::S3Store;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Small cap for the second tombstone-conformance run, so cap truncation
/// (`ancestors_capped_and_ordered`) is exercised in a handful of writes.
const SMALL_CAP: usize = 3;

/// `(endpoint, bucket)` from the env, or `None` (skip) when unset.
fn test_target() -> Option<(String, String)> {
    match (
        std::env::var("GONZALO_S3_TEST_ENDPOINT"),
        std::env::var("GONZALO_S3_TEST_BUCKET"),
    ) {
        (Ok(e), Ok(b)) => Some((e, b)),
        _ => {
            eprintln!("skipping: set GONZALO_S3_TEST_ENDPOINT and GONZALO_S3_TEST_BUCKET to run");
            None
        }
    }
}

/// Optional SDK region. `scripts/rustfs-up.sh` exports `GONZALO_S3_TEST_REGION`
/// rather than `AWS_REGION`, and SigV4 signing needs one.
fn test_region() -> Option<String> {
    std::env::var("GONZALO_S3_TEST_REGION")
        .ok()
        .filter(|r| !r.trim().is_empty())
}

static BUCKET_SEQ: AtomicU64 = AtomicU64::new(0);

/// A store over a brand-new, empty bucket. A conformance factory must return a
/// fresh empty store on every call: the cases use fixed keys, assert `get_raw`
/// of an absent key is `None`, and compare deletes across two stores. One
/// shared bucket can't give that across cases or reruns. Buckets are left
/// behind; `scripts/rustfs-down.sh --purge` drops the volume.
async fn fresh_bucket_store(endpoint: &str) -> S3Store {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let bucket = format!(
        "gz-test-{}-{nanos}-{}",
        std::process::id(),
        BUCKET_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let base = aws_config::load_from_env().await;
    let mut builder = aws_sdk_s3::config::Builder::from(&base)
        .endpoint_url(endpoint)
        .force_path_style(true);
    if let Some(r) = test_region() {
        builder = builder.region(aws_sdk_s3::config::Region::new(r));
    }
    let client = aws_sdk_s3::Client::from_conf(builder.build());
    if let Err(e) = client.create_bucket().bucket(&bucket).send().await {
        panic!("create bucket {bucket}: {}", e.into_service_error());
    }
    S3Store::new(client, bucket)
}

fn sample(key: RecordKey, payload: &[u8], revision: Revision, parent: Option<Revision>) -> Record {
    let body = Body::Inline(payload.to_vec());
    Record {
        revision,
        parent,
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
        ancestors: Vec::new(),
        deleted_at: None,
        key,
    }
}

/// Seed `key` in `store` and return the committed base revision.
async fn seed(store: &S3Store, key: &RecordKey) -> Revision {
    match store
        .put(
            sample(key.clone(), b"v1", Revision::initial(b"v1"), None),
            None,
        )
        .await
        .unwrap()
    {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("a fresh bucket must accept the seed"),
    }
}

#[tokio::test]
async fn s3_store_passes_conformance_when_endpoint_configured() {
    let Some((endpoint, _bucket)) = test_target() else {
        return;
    };
    let endpoint = endpoint.as_str();
    run_store_conformance(move || fresh_bucket_store(endpoint)).await;
}

/// Spec §6.1 tombstone cases at the default cap (RustFS-qualified, ADR 0019).
#[tokio::test]
async fn s3_store_passes_tombstone_conformance_at_default_cap() {
    let Some((endpoint, _bucket)) = test_target() else {
        return;
    };
    let endpoint = endpoint.as_str();
    run_tombstone_conformance(move || fresh_bucket_store(endpoint), DEFAULT_ANCESTOR_CAP).await;
}

/// The same cases at a small cap, so truncation is exercised.
#[tokio::test]
async fn s3_store_passes_tombstone_conformance_at_small_cap() {
    let Some((endpoint, _bucket)) = test_target() else {
        return;
    };
    let endpoint = endpoint.as_str();
    run_tombstone_conformance(
        move || async move {
            fresh_bucket_store(endpoint)
                .await
                .with_ancestor_cap(SMALL_CAP)
                .expect("a cap of 3 is valid")
        },
        SMALL_CAP,
    )
    .await;
}

/// Live coverage of the S3 `BlobStore` impl (gonzalo#62): put/get/list/delete
/// against MinIO. Self-cleaning and asserts on membership rather than a global
/// empty set, so it doesn't depend on a pristine bucket (which the shared
/// `run_blob_store_conformance` — designed for fresh-per-call stores — assumes).
#[tokio::test]
async fn s3_blob_store_put_get_list_delete() {
    let Some((endpoint, bucket)) = test_target() else {
        return;
    };
    let store = S3Store::connect(bucket, Some(endpoint), test_region()).await;

    let content = b"content-addressed slice for #62";
    let hash = store.put_blob(content).await.unwrap();
    assert_eq!(hash, ContentHash::of(content), "hash is content-addressed");

    // Round-trips.
    assert_eq!(
        store.get_blob(&hash).await.unwrap().as_deref(),
        Some(&content[..])
    );

    // Idempotent re-put yields the same hash and leaves content intact.
    assert_eq!(store.put_blob(content).await.unwrap(), hash);
    assert_eq!(
        store.get_blob(&hash).await.unwrap().as_deref(),
        Some(&content[..])
    );

    // Listed among the stored blobs.
    assert!(
        store.list_blobs().await.unwrap().contains(&hash),
        "put blob must appear in list_blobs"
    );

    // Delete removes it and is idempotent.
    store.delete_blob(&hash).await.unwrap();
    assert_eq!(store.get_blob(&hash).await.unwrap(), None);
    store.delete_blob(&hash).await.unwrap();
    assert!(!store.list_blobs().await.unwrap().contains(&hash));
}

/// The TOCTOU acceptance test for gonzalo#5: many writers that all read the
/// same `expected` revision then race to update. Native conditional writes
/// (`If-Match`) must let **exactly one** commit. Each loser's 412 re-reads and
/// re-plans against the winner's revision, which is a recoverable `Conflict`.
/// Without conditional writes the read-then-write window lets several
/// "commit" and silently clobber.
#[tokio::test]
async fn concurrent_updates_with_same_expected_let_exactly_one_win() {
    let Some((endpoint, bucket)) = test_target() else {
        return;
    };
    let key = RecordKey::new("race", "col", "one");

    // Seed the object and capture the revision every racer will hold.
    let store = S3Store::connect(bucket.clone(), Some(endpoint.clone()), test_region()).await;
    let v1 = sample(key.clone(), b"v1", Revision::initial(b"v1"), None);
    // Best-effort clean slate if a prior run left the key behind.
    let base_rev = loop {
        match store.put(v1.clone(), None).await.unwrap() {
            PutResult::Committed(rev) => break rev,
            PutResult::Conflict(c) => {
                // Overwrite whatever is there back to a known v1.
                let reset = sample(
                    key.clone(),
                    b"v1",
                    c.current.revision.next(b"v1"),
                    Some(c.current.revision.clone()),
                );
                if let PutResult::Committed(rev) =
                    store.put(reset, Some(c.current.revision)).await.unwrap()
                {
                    break rev;
                }
            }
        }
    };

    // Fan out N concurrent updaters, each holding `base_rev`.
    let n = 8;
    let mut handles = Vec::new();
    for i in 0..n {
        let (endpoint, bucket, key, base_rev) = (
            endpoint.clone(),
            bucket.clone(),
            key.clone(),
            base_rev.clone(),
        );
        handles.push(tokio::spawn(async move {
            let store = S3Store::connect(bucket, Some(endpoint), test_region()).await;
            let payload = format!("racer-{i}");
            let rec = sample(
                key,
                payload.as_bytes(),
                base_rev.next(payload.as_bytes()),
                Some(base_rev.clone()),
            );
            store.put(rec, Some(base_rev)).await.unwrap()
        }));
    }

    let mut committed = 0;
    let mut conflicts = 0;
    for h in handles {
        match h.await.unwrap() {
            PutResult::Committed(_) => committed += 1,
            PutResult::Conflict(_) => conflicts += 1,
        }
    }
    assert_eq!(
        committed, 1,
        "exactly one racer may commit (got {committed})"
    );
    assert_eq!(conflicts, n - 1, "the rest must conflict (got {conflicts})");
}

/// What one updater in the race tests got.
#[derive(Debug)]
enum PutOutcome {
    Committed,
    Conflict,
    NotFound,
}

async fn racing_update(
    store: Arc<S3Store>,
    key: RecordKey,
    base_rev: Revision,
    i: usize,
) -> PutOutcome {
    let payload = format!("updater-{i}");
    let rec = sample(
        key,
        payload.as_bytes(),
        base_rev.next(payload.as_bytes()),
        Some(base_rev.clone()),
    );
    match store.put(rec, Some(base_rev)).await {
        Ok(PutResult::Committed(_)) => PutOutcome::Committed,
        Ok(PutResult::Conflict(_)) => PutOutcome::Conflict,
        Err(CoreError::NotFound(_)) => PutOutcome::NotFound,
        Err(e) => panic!("unexpected put error: {e}"),
    }
}

/// Conditional deletes and updates that all hold one base revision. Exactly
/// one *kind* of write wins. If a delete wins, every deleter reports `Deleted`
/// (the rest re-plan onto the tombstone, a no-op) and every updater re-plans
/// onto the tombstone (`NotFound`). If an update wins, exactly one updater
/// commits and every deleter re-plans onto the new revision (`Conflict`). A
/// mix means the conditional tombstone write wasn't atomic.
#[tokio::test]
async fn racing_deletes_and_updates_on_one_revision_stay_atomic() {
    let Some((endpoint, _bucket)) = test_target() else {
        return;
    };
    let store = Arc::new(fresh_bucket_store(&endpoint).await);
    let key = RecordKey::new("race", "col", "delete-vs-update");
    let base_rev = seed(&store, &key).await;

    let per_kind = 4;
    let mut deleters = Vec::new();
    let mut updaters = Vec::new();
    for i in 0..per_kind {
        let (s, k, b) = (store.clone(), key.clone(), base_rev.clone());
        deleters.push(tokio::spawn(
            async move { s.delete(&k, Some(b)).await.unwrap() },
        ));
        let (s, k, b) = (store.clone(), key.clone(), base_rev.clone());
        updaters.push(tokio::spawn(racing_update(s, k, b, i)));
    }

    let (mut deleted, mut delete_conflicts) = (0, 0);
    for h in deleters {
        match h.await.unwrap() {
            DeleteResult::Deleted => deleted += 1,
            DeleteResult::Conflict(_) => delete_conflicts += 1,
        }
    }
    let (mut committed, mut put_conflicts, mut put_not_found) = (0, 0, 0);
    for h in updaters {
        match h.await.unwrap() {
            PutOutcome::Committed => committed += 1,
            PutOutcome::Conflict => put_conflicts += 1,
            PutOutcome::NotFound => put_not_found += 1,
        }
    }

    let raw = store
        .get_raw(&key)
        .await
        .unwrap()
        .expect("the key is still stored (tombstone or live)");
    if raw.is_tombstone() {
        assert_eq!(committed, 0, "no update may commit over a winning delete");
        assert_eq!(deleted, per_kind, "every deleter sees the key gone");
        assert_eq!(delete_conflicts, 0);
        assert_eq!(put_not_found, per_kind, "every updater sees a tombstone");
        assert_eq!(raw.revision.counter, base_rev.counter + 1);
        assert_eq!(store.get(&key).await.unwrap(), None);
    } else {
        assert_eq!(committed, 1, "exactly one updater commits");
        assert_eq!(put_conflicts, per_kind - 1);
        assert_eq!(delete_conflicts, per_kind, "every deleter conflicts");
        assert_eq!(deleted, 0);
    }
}

/// Unconditional deletes never conflict because they lost a race: a 412
/// re-plans against whatever is now current and tombstones it, as the fs and
/// git stores do under their locks. So every deleter reports `Deleted`, the
/// key always ends tombstoned, and at most one updater committed first.
#[tokio::test]
async fn unconditional_deletes_racing_updates_always_end_deleted() {
    let Some((endpoint, _bucket)) = test_target() else {
        return;
    };
    let store = Arc::new(fresh_bucket_store(&endpoint).await);
    let key = RecordKey::new("race", "col", "unconditional-delete");
    let base_rev = seed(&store, &key).await;

    let per_kind = 4;
    let mut deleters = Vec::new();
    let mut updaters = Vec::new();
    for i in 0..per_kind {
        let (s, k) = (store.clone(), key.clone());
        deleters.push(tokio::spawn(
            async move { s.delete(&k, None).await.unwrap() },
        ));
        let (s, k, b) = (store.clone(), key.clone(), base_rev.clone());
        updaters.push(tokio::spawn(racing_update(s, k, b, i)));
    }

    for h in deleters {
        assert_eq!(
            h.await.unwrap(),
            DeleteResult::Deleted,
            "an unconditional delete must never conflict"
        );
    }
    let mut committed = 0;
    for h in updaters {
        if let PutOutcome::Committed = h.await.unwrap() {
            committed += 1;
        }
    }
    assert!(
        committed <= 1,
        "at most one updater commits (got {committed})"
    );

    let raw = store
        .get_raw(&key)
        .await
        .unwrap()
        .expect("the key ends as a tombstone");
    assert!(raw.is_tombstone(), "the key must end deleted");
    // The tombstone sits on the update if one landed first, else on the seed.
    assert_eq!(raw.revision.counter, base_rev.counter + 1 + committed);
    assert_eq!(store.get(&key).await.unwrap(), None);
}

/// Purges naming a tombstone's revision race recreations of the key. The
/// conditional `DeleteObject` must be atomic: exactly one recreator commits,
/// and that record survives every purge. A missing key or a tombstone at the
/// end, after a commit, means `If-Match` on `DeleteObject` was not enforced.
#[tokio::test]
async fn purge_racing_recreation_never_removes_the_recreated_record() {
    let Some((endpoint, _bucket)) = test_target() else {
        return;
    };
    let store = Arc::new(fresh_bucket_store(&endpoint).await);
    let key = RecordKey::new("race", "col", "purge-vs-recreate");
    let _ = seed(&store, &key).await;
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );
    let tomb = store
        .get_raw(&key)
        .await
        .unwrap()
        .expect("the delete wrote a tombstone");

    let per_kind = 4;
    let mut purgers = Vec::new();
    let mut recreators = Vec::new();
    for i in 0..per_kind {
        let (s, k, r) = (store.clone(), key.clone(), tomb.revision.clone());
        purgers.push(tokio::spawn(async move { s.purge(&k, r).await.unwrap() }));
        let (s, k) = (store.clone(), key.clone());
        recreators.push(tokio::spawn(async move {
            let payload = format!("recreate-{i}");
            let rec = sample(
                k,
                payload.as_bytes(),
                Revision::initial(payload.as_bytes()),
                None,
            );
            s.put(rec, None).await.unwrap()
        }));
    }

    for h in purgers {
        // `Deleted` (purged before any recreation) and `Conflict` are both legal.
        let _ = h.await.unwrap();
    }
    let mut committed = Vec::new();
    for h in recreators {
        if let PutResult::Committed(rev) = h.await.unwrap() {
            committed.push(rev);
        }
    }
    assert_eq!(
        committed.len(),
        1,
        "exactly one recreator commits: {committed:?}"
    );
    let raw = store
        .get_raw(&key)
        .await
        .unwrap()
        .expect("a committed recreation must survive every purge");
    assert!(!raw.is_tombstone(), "the recreated record must be live");
    assert_eq!(raw.revision, committed[0]);
}
