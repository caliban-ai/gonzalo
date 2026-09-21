//! A [`VectorIndex`] whose contents survive the process that built it (ADR 0027).
//!
//! Vectors live in content-addressed shard blobs named by one
//! [`VectorManifest`] record. Queries are served from an in-memory
//! [`MemoryVectorIndex`] hydrated at [`open`](RecordVectorIndex::open); writes
//! go through to the store under OCC.
//!
//! **One writer per index.** A concurrent writer is detected and retried, not
//! merged, and a second writer's in-memory view can be stale until it reopens.

use crate::shard::{DEFAULT_SHARDS, decode_shard, shard_of};
use crate::{Match, MemoryVectorIndex, VectorIndex};
use async_trait::async_trait;
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, Identity, KeyPrefix, Meta, RecordKey, Result, Store,
    VectorManifest,
};
use std::num::NonZeroU16;
use std::sync::Arc;

/// How many shard blobs to read at once when opening. Matches
/// `LIST_READ_CONCURRENCY` in `gonzalo-store-s3`, which settled on 16 for the
/// same reason: enough parallelism to hide latency, not enough to thrash.
const SHARD_READ_CONCURRENCY: usize = 16;

/// How many times a commit re-reads and retries before giving up.
///
/// Unused until Task 6 wires up the write path; kept here now so the retry
/// budget lives beside the manifest-loading code it will govern.
#[expect(dead_code)]
const MAX_COMMIT_ATTEMPTS: usize = 5;

/// One pending change to apply and persist.
///
/// Unused until Task 6 wires up the write path.
#[expect(dead_code)]
#[derive(Clone, Debug)]
enum Delta {
    Upsert(RecordKey, Vec<f32>),
    Remove(RecordKey),
}

/// The error a shard id names in its manifest entry but whose blob is absent.
/// Shared between the single-shard reload path and the concurrent open path
/// so both name the same failure the same way.
fn missing_blob_error(key: &RecordKey, id: u16, hash: &ContentHash) -> CoreError {
    CoreError::Backend(format!(
        "vector index {key}: shard {id} names blob {} but it is absent",
        hash.0
    ))
}

pub struct RecordVectorIndex<S> {
    store: Arc<S>,
    key: RecordKey,
    space: String,
    dim: usize,
    shards: NonZeroU16,
    inner: MemoryVectorIndex,
    /// The identity/origin stamped on manifest commits. Unread until Task 6
    /// wires up the write path.
    #[expect(dead_code)]
    meta: Meta,
}

impl<S> std::fmt::Debug for RecordVectorIndex<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordVectorIndex")
            .field("key", &self.key)
            .field("space", &self.space)
            .field("dim", &self.dim)
            .field("shards", &self.shards)
            .finish_non_exhaustive()
    }
}

impl<S: Store + BlobStore + Send + Sync + 'static> RecordVectorIndex<S> {
    /// Open the index at `key`, hydrating it from `store`.
    ///
    /// Errors if a manifest exists whose `space` or `dim` differs from the
    /// declared one, naming both values, so a swapped embedder surfaces once at
    /// startup rather than as quietly wrong rankings on every later query. A
    /// missing manifest starts an empty index created on the first commit.
    pub async fn open(store: S, key: RecordKey, space: &str, dim: usize) -> Result<Self> {
        Self::open_with_shards(store, key, space, dim, DEFAULT_SHARDS).await
    }

    /// As [`open`](Self::open), but choosing the shard count for a **new**
    /// index. An existing manifest's shard count always wins.
    pub async fn open_with_shards(
        store: S,
        key: RecordKey,
        space: &str,
        dim: usize,
        shards: NonZeroU16,
    ) -> Result<Self> {
        let store = Arc::new(store);
        let existing = store.get(&key).await?;
        let (shards, manifest) = match &existing {
            None => (shards, None),
            Some(record) => {
                let m = VectorManifest::from_body(&record.body)?;
                if m.space != space {
                    return Err(CoreError::Invalid(format!(
                        "vector index {key}: stored embedding space is {:?}, opened as {:?}",
                        m.space, space
                    )));
                }
                if m.dim != dim {
                    return Err(CoreError::Invalid(format!(
                        "vector index {key}: stored dimension is {}, opened as {}",
                        m.dim, dim
                    )));
                }
                // A stored shard count of zero means the manifest is corrupt.
                // Surface it here rather than letting it reach `shard_of`.
                let stored = NonZeroU16::new(m.shards).ok_or_else(|| {
                    CoreError::Backend(format!("vector index {key}: manifest declares 0 shards"))
                })?;
                (stored, Some(m))
            }
        };

        let index = Self {
            store,
            key,
            space: space.to_string(),
            dim,
            shards,
            inner: MemoryVectorIndex::new(),
            meta: Meta::new(Identity::new("gonzalo-vector"), "gonzalo-vector"),
        };

        if let Some(m) = manifest {
            let ids: Vec<u16> = m.entries.keys().copied().collect();
            for batch in ids.chunks(SHARD_READ_CONCURRENCY) {
                // Fetch phase: every read in the batch runs concurrently. A
                // handle per task — an `Arc` clone is cheap, and a spawned task
                // needs to own what it reads (`FsStore` is not `Clone`, and
                // `store.get_blob` needs `&self` for the batch's whole
                // lifetime otherwise). Mirrors `read_liveness` in
                // `gonzalo-store-s3` (gonzalo#286).
                let mut reads = tokio::task::JoinSet::new();
                for &id in batch {
                    let store = Arc::clone(&index.store);
                    let hash = m
                        .entries
                        .get(&id)
                        .expect("id came from these entries' own keys")
                        .clone();
                    let index_key = index.key.clone();
                    reads.spawn(async move {
                        let bytes = store
                            .get_blob(&hash)
                            .await?
                            .ok_or_else(|| missing_blob_error(&index_key, id, &hash))?;
                        Ok::<_, CoreError>((id, bytes))
                    });
                }

                // Apply phase: decode and mutate `index.inner` sequentially, on
                // the collected results, so there is no interleaving of writes
                // to reason about.
                let mut fetched = Vec::with_capacity(batch.len());
                while let Some(joined) = reads.join_next().await {
                    fetched.push(joined.map_err(|e| CoreError::Backend(e.to_string()))??);
                }
                for (id, bytes) in fetched {
                    index.apply_shard_bytes(id, &bytes).await?;
                }
            }
        }
        Ok(index)
    }

    /// Borrow the underlying store.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Load one shard from `manifest` into the in-memory index, replacing
    /// whatever that shard currently holds.
    ///
    /// Kept for Task 6's single-shard reload after a conflicting write; that
    /// path re-reads one shard at a time and gets nothing from the batch
    /// concurrency `open_with_shards` uses.
    #[expect(dead_code)]
    async fn hydrate_shard(&self, manifest: &VectorManifest, id: u16) -> Result<()> {
        let Some(hash) = manifest.entries.get(&id) else {
            return self.clear_shard(id).await;
        };
        let bytes = self
            .store
            .get_blob(hash)
            .await?
            .ok_or_else(|| missing_blob_error(&self.key, id, hash))?;
        self.apply_shard_bytes(id, &bytes).await
    }

    /// Remove every entry currently held in-memory for shard `id`.
    async fn clear_shard(&self, id: u16) -> Result<()> {
        for (key, _) in self.inner.collect_where(|k| shard_of(k, self.shards) == id) {
            self.inner.remove(&key).await?;
        }
        Ok(())
    }

    /// Decode `bytes` as shard `id` and replace that shard's entries in the
    /// in-memory index.
    async fn apply_shard_bytes(&self, id: u16, bytes: &[u8]) -> Result<()> {
        let (dim, entries) = decode_shard(bytes)?;
        if dim != self.dim {
            return Err(CoreError::Backend(format!(
                "vector index {}: shard {id} has dimension {dim}, manifest says {}",
                self.key, self.dim
            )));
        }
        self.clear_shard(id).await?;
        self.inner.upsert_many(entries).await
    }
}

#[async_trait]
impl<S: Store + BlobStore> VectorIndex for RecordVectorIndex<S> {
    async fn upsert(&self, _key: RecordKey, _vector: Vec<f32>) -> Result<()> {
        Err(CoreError::Backend("not yet implemented".into()))
    }

    async fn remove(&self, _key: &RecordKey) -> Result<()> {
        Err(CoreError::Backend("not yet implemented".into()))
    }

    async fn query(&self, query: &[f32], k: usize, filter: &KeyPrefix) -> Result<Vec<Match>> {
        self.inner.query(query, k, filter).await
    }

    async fn keys(&self, filter: &KeyPrefix) -> Result<Vec<RecordKey>> {
        self.inner.keys(filter).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_core::{
        BlobStore, ContentHash, DeleteResult, Identity, Meta, PutResult, Record, RecordKind,
        Revision, Store,
    };
    use gonzalo_store_fs::FsStore;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        TempDir::new().unwrap()
    }

    /// A fresh handle onto the same directory. `FsStore` is not `Clone`, and a
    /// second handle is what a restart actually looks like anyway.
    fn fs(dir: &TempDir) -> FsStore {
        FsStore::new(dir.path())
    }

    /// Wraps a real `FsStore` and instruments `get_blob` so a test can observe
    /// how many reads were in flight at once, proving (or disproving) that
    /// `open` actually overlaps its shard reads rather than merely claiming to.
    struct CountingStore {
        inner: FsStore,
        in_flight: std::sync::Arc<AtomicUsize>,
        max_in_flight: std::sync::Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Store for CountingStore {
        async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.inner.get(key).await
        }
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            self.inner.put(record, expected).await
        }
        async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            self.inner.list(prefix).await
        }
        async fn delete_as(
            &self,
            key: &RecordKey,
            expected: Option<Revision>,
            author: Option<Identity>,
        ) -> Result<DeleteResult> {
            self.inner.delete_as(key, expected, author).await
        }
        async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.inner.get_raw(key).await
        }
        async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            self.inner.list_raw(prefix).await
        }
        async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            self.inner.put_raw(record, expected).await
        }
        async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
            self.inner.purge(key, expected).await
        }
    }

    #[async_trait]
    impl BlobStore for CountingStore {
        async fn put_blob(&self, content: &[u8]) -> Result<ContentHash> {
            self.inner.put_blob(content).await
        }

        async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
            let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(current, Ordering::SeqCst);
            // Long enough that every task in a batch has had the chance to
            // record its own increment before any of them proceeds, so the
            // observed maximum reflects real overlap, not a lucky race.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let result = self.inner.get_blob(hash).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        }

        async fn list_blobs(&self) -> Result<Vec<ContentHash>> {
            self.inner.list_blobs().await
        }

        async fn delete_blob(&self, hash: &ContentHash) -> Result<()> {
            self.inner.delete_blob(hash).await
        }
    }

    fn index_key() -> RecordKey {
        VectorManifest::key("ns", "memories")
    }

    #[tokio::test]
    async fn opening_a_missing_index_starts_empty() {
        let dir = tmp();
        let s = fs(&dir);
        let idx = RecordVectorIndex::open(s, index_key(), "space-a", 3)
            .await
            .unwrap();
        assert!(idx.keys(&KeyPrefix::default()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn opening_hydrates_vectors_from_shard_blobs() {
        let dir = tmp();
        let s = fs(&dir);
        let k = RecordKey::new("ns", "coll", "a");

        // Hand-build a one-shard index so the load path is tested without
        // depending on the write path, which does not exist yet.
        let entries = vec![(k.clone(), vec![1.0, 0.0, 0.0])];
        let hash = s
            .put_blob(&crate::shard::encode_shard(3, &entries))
            .await
            .unwrap();
        let mut vm = VectorManifest::new("space-a", 3, DEFAULT_SHARDS.get());
        vm.entries.insert(shard_of(&k, DEFAULT_SHARDS), hash);
        let rec = Record::create(
            index_key(),
            RecordKind::VectorManifest,
            vm.to_body(),
            Meta::new(Identity::new("test"), "test"),
        );
        assert!(matches!(
            s.put(rec, None).await.unwrap(),
            PutResult::Committed(_)
        ));

        let idx = RecordVectorIndex::open(s, index_key(), "space-a", 3)
            .await
            .unwrap();
        let hits = idx
            .query(&[1.0, 0.0, 0.0], 5, &KeyPrefix::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, k);
    }

    // The realistic failure this guards: a swapped embedder. Two models can share
    // a dimension (all-MiniLM-L6-v2 and bge-small are both 384), so dimension
    // alone would let mismatched vectors score against each other forever.
    #[tokio::test]
    async fn opening_with_a_different_space_errors_naming_both() {
        let dir = tmp();
        let s = fs(&dir);
        let vm = VectorManifest::new("space-a", 3, DEFAULT_SHARDS.get());
        let rec = Record::create(
            index_key(),
            RecordKind::VectorManifest,
            vm.to_body(),
            Meta::new(Identity::new("test"), "test"),
        );
        assert!(matches!(
            s.put(rec, None).await.unwrap(),
            PutResult::Committed(_)
        ));

        let err = RecordVectorIndex::open(s, index_key(), "space-b", 3)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("space-a"), "stored space missing from: {err}");
        assert!(
            err.contains("space-b"),
            "declared space missing from: {err}"
        );
    }

    #[tokio::test]
    async fn opening_with_a_different_dim_errors_naming_both() {
        let dir = tmp();
        let s = fs(&dir);
        let vm = VectorManifest::new("space-a", 3, DEFAULT_SHARDS.get());
        let rec = Record::create(
            index_key(),
            RecordKind::VectorManifest,
            vm.to_body(),
            Meta::new(Identity::new("test"), "test"),
        );
        assert!(matches!(
            s.put(rec, None).await.unwrap(),
            PutResult::Committed(_)
        ));

        let err = RecordVectorIndex::open(s, index_key(), "space-a", 8)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains('3'), "stored dim missing from: {err}");
        assert!(err.contains('8'), "declared dim missing from: {err}");
    }

    // A manifest naming a blob that is gone means gc swept a live blob or a write
    // was lost. Opening short and quiet would turn that into missing search hits.
    #[tokio::test]
    async fn a_missing_shard_blob_is_a_loud_error() {
        let dir = tmp();
        let s = fs(&dir);
        let mut vm = VectorManifest::new("space-a", 3, DEFAULT_SHARDS.get());
        vm.entries
            .insert(7, gonzalo_core::ContentHash::of(b"never stored"));
        let rec = Record::create(
            index_key(),
            RecordKind::VectorManifest,
            vm.to_body(),
            Meta::new(Identity::new("test"), "test"),
        );
        assert!(matches!(
            s.put(rec, None).await.unwrap(),
            PutResult::Committed(_)
        ));

        let err = RecordVectorIndex::open(s, index_key(), "space-a", 3)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains('7'), "shard id missing from: {err}");
    }

    // #323 fix-round-1: `SHARD_READ_CONCURRENCY` claimed shard reads on open
    // overlap. A chunked-but-sequential loop would make that false while still
    // naming the s3 constant it was modeled on, which is worse than an honest
    // sequential loop. Prove the overlap actually happens rather than trusting
    // the doc comment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn opening_reads_shard_blobs_concurrently() {
        let dir = tmp();
        let inner = fs(&dir);
        let in_flight = std::sync::Arc::new(AtomicUsize::new(0));
        let max_in_flight = std::sync::Arc::new(AtomicUsize::new(0));
        let s = CountingStore {
            inner,
            in_flight: std::sync::Arc::clone(&in_flight),
            max_in_flight: std::sync::Arc::clone(&max_in_flight),
        };

        // Several independent shard blobs, so the open path has more than one
        // blob to fetch and a real chance to overlap them.
        let mut vm = VectorManifest::new("space-a", 3, DEFAULT_SHARDS.get());
        for shard_id in 0..4u16 {
            let k = RecordKey::new("ns", "coll", format!("k{shard_id}"));
            let entries = vec![(k, vec![1.0, 0.0, 0.0])];
            let hash = s
                .put_blob(&crate::shard::encode_shard(3, &entries))
                .await
                .unwrap();
            vm.entries.insert(shard_id, hash);
        }
        let rec = Record::create(
            index_key(),
            RecordKind::VectorManifest,
            vm.to_body(),
            Meta::new(Identity::new("test"), "test"),
        );
        assert!(matches!(
            s.put(rec, None).await.unwrap(),
            PutResult::Committed(_)
        ));

        RecordVectorIndex::open(s, index_key(), "space-a", 3)
            .await
            .unwrap();

        let observed = max_in_flight.load(Ordering::SeqCst);
        assert!(
            observed > 1,
            "expected overlapping shard reads, saw a max in-flight of {observed}"
        );
    }
}
