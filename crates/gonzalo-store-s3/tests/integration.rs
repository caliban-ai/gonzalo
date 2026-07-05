use gonzalo_core::conformance::run_store_conformance;
use gonzalo_core::{
    BlobStore, Body, ContentHash, Identity, Meta, PutResult, Record, RecordKey, RecordKind,
    Revision, Store,
};
use gonzalo_store_s3::S3Store;
use std::collections::BTreeMap;

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
        key,
    }
}

#[tokio::test]
async fn s3_store_passes_conformance_when_endpoint_configured() {
    let Some((endpoint, bucket)) = test_target() else {
        return;
    };
    run_store_conformance(|| async {
        S3Store::connect(bucket.clone(), Some(endpoint.clone()), None).await
    })
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
    let store = S3Store::connect(bucket, Some(endpoint), None).await;

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
/// (`If-Match`) must let **exactly one** commit; the rest lose the race with a
/// 412 that surfaces as a recoverable `Conflict`. Without conditional writes
/// the read-then-write window lets several "commit" and silently clobber.
#[tokio::test]
async fn concurrent_updates_with_same_expected_let_exactly_one_win() {
    let Some((endpoint, bucket)) = test_target() else {
        return;
    };
    let key = RecordKey::new("race", "col", "one");

    // Seed the object and capture the revision every racer will hold.
    let store = S3Store::connect(bucket.clone(), Some(endpoint.clone()), None).await;
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
            let store = S3Store::connect(bucket, Some(endpoint), None).await;
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
