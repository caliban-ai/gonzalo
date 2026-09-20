//! Measure `Store::list` over a collection of N live records and M tombstones
//! (gonzalo#294, ADR 0025). Run it against RustFS:
//!
//! ```sh
//! eval "$(scripts/rustfs-up.sh)"
//! cargo run -p gonzalo-store-s3 --release --example list_bench -- 500 200
//! ```
//!
//! It seeds a fresh bucket, then lists three times:
//!
//! 1. **unflagged** — the pre-ADR-0025 cost, and what the first listing after
//!    an upgrade pays: one `GetObject` per key, N+M of them, plus the markers
//!    it backfills;
//! 2. **flagged** — the new steady state: one listing pass and a read per
//!    *tombstone*, M of them;
//! 3. **collected** — the same after the tombstones are purged, which is what
//!    `collect` leaves behind: one listing pass and no reads at all.
//!
//! The read counts are the durable result; the wall-clock depends on the
//! machine and the S3 implementation, and against a loopback RustFS it
//! understates the win badly — the cost this removes is a network round trip
//! per record, not local work.

use gonzalo_core::{
    Body, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind,
    Revision, Store,
};
use gonzalo_store_s3::S3Store;
use std::collections::BTreeMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const NAMESPACE: &str = "bench";
const COLLECTION: &str = "records";

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let live: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(500);
    let dead: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(200);

    let endpoint = std::env::var("GONZALO_S3_TEST_ENDPOINT")
        .expect("set GONZALO_S3_TEST_ENDPOINT (eval \"$(scripts/rustfs-up.sh)\")");
    let store = fresh_bucket(&endpoint).await;

    println!("seeding {live} live records and {dead} tombstones…");
    for i in 0..live {
        seed(&store, &key(&format!("live-{i:05}"))).await;
    }
    let mut tombstoned = Vec::with_capacity(dead);
    for i in 0..dead {
        let k = key(&format!("dead-{i:05}"));
        let rev = seed(&store, &k).await;
        assert_eq!(
            store.delete(&k, Some(rev)).await.unwrap(),
            DeleteResult::Deleted
        );
        tombstoned.push(k);
    }

    // The pre-markers cost. The collection is unflagged, so `list` reads every
    // key — and, while it is already reading them, backfills and flags.
    let (unflagged, n) = timed(&store).await;
    assert_eq!(n, live, "the tombstones must be hidden");
    println!("  unflagged (reads {}): {unflagged:?}", live + dead);

    // The steady state: one pass, plus a read per tombstone.
    let (flagged, n) = timed(&store).await;
    assert_eq!(n, live);
    println!("  flagged   (reads {dead}): {flagged:?}");

    // After `collect`: the tombstones and their markers are gone, so a listing
    // reads nothing at all.
    for k in &tombstoned {
        let tomb = store.get_raw(k).await.unwrap().expect("tombstone");
        assert_eq!(
            store.purge(k, tomb.revision).await.unwrap(),
            DeleteResult::Deleted
        );
    }
    let (collected, n) = timed(&store).await;
    assert_eq!(n, live);
    println!("  collected (reads 0): {collected:?}");
}

/// One `list` over the seeded collection: how long it took, and how many keys
/// it returned.
async fn timed(store: &S3Store) -> (std::time::Duration, usize) {
    let prefix = KeyPrefix {
        namespace: Some(NAMESPACE.into()),
        collection: Some(COLLECTION.into()),
    };
    let started = Instant::now();
    let listed = store.list(&prefix).await.expect("list");
    (started.elapsed(), listed.len())
}

fn key(id: &str) -> RecordKey {
    RecordKey::new(NAMESPACE, COLLECTION, id)
}

async fn seed(store: &S3Store, key: &RecordKey) -> Revision {
    let body = Body::Inline(b"benchmark payload".to_vec());
    let record = Record {
        revision: Revision::initial(body.bytes()),
        parent: None,
        body,
        kind: RecordKind::Topic,
        meta: Meta {
            author: Identity::new("bench"),
            origin_system: "bench".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        ancestors: Vec::new(),
        deleted_at: None,
        deleted_blob: None,
        key: key.clone(),
    };
    match store.put(record, None).await.expect("put") {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    }
}

async fn fresh_bucket(endpoint: &str) -> S3Store {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let bucket = format!("gz-bench-{}-{nanos}", std::process::id());
    let base = aws_config::load_from_env().await;
    let mut builder = aws_sdk_s3::config::Builder::from(&base)
        .endpoint_url(endpoint)
        .force_path_style(true);
    if let Ok(region) = std::env::var("GONZALO_S3_TEST_REGION")
        && !region.trim().is_empty()
    {
        builder = builder.region(aws_sdk_s3::config::Region::new(region));
    }
    let client = aws_sdk_s3::Client::from_conf(builder.build());
    client
        .create_bucket()
        .bucket(&bucket)
        .send()
        .await
        .expect("create bucket");
    println!("bucket: {bucket}");
    S3Store::new(client, bucket)
}
