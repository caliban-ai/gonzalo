//! The ticket's acceptance criterion: an index of real size is written, dropped,
//! reopened, and queried — with the reopen cost reported rather than assumed.

use gonzalo_core::{KeyPrefix, RecordKey, VectorManifest};
use gonzalo_store_fs::FsStore;
use gonzalo_vector::{RecordVectorIndex, VectorIndex as _};
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
