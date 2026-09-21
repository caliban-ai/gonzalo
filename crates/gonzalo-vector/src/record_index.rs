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
    BlobStore, CoreError, Identity, KeyPrefix, Meta, RecordKey, Result, Store, VectorManifest,
};
use std::num::NonZeroU16;

/// How many shard blobs to read at once when opening. Matches
/// `LIST_READ_CONCURRENCY` in `gonzalo-store-s3`, which settled on 16 for the
/// same reason: enough parallelism to hide latency, not enough to thrash.
const SHARD_READ_CONCURRENCY: usize = 16;

/// How many times a commit re-reads and retries before giving up.
///
/// Unused until Task 6 wires up the write path; kept here now so the retry
/// budget lives beside the manifest-loading code it will govern.
#[allow(dead_code)]
const MAX_COMMIT_ATTEMPTS: usize = 5;

/// One pending change to apply and persist.
///
/// Unused until Task 6 wires up the write path.
#[allow(dead_code)]
#[derive(Clone, Debug)]
enum Delta {
    Upsert(RecordKey, Vec<f32>),
    Remove(RecordKey),
}

pub struct RecordVectorIndex<S> {
    store: S,
    key: RecordKey,
    space: String,
    dim: usize,
    shards: NonZeroU16,
    inner: MemoryVectorIndex,
    /// The identity/origin stamped on manifest commits. Unread until Task 6
    /// wires up the write path.
    #[allow(dead_code)]
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

impl<S: Store + BlobStore> RecordVectorIndex<S> {
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
                for &id in batch {
                    index.hydrate_shard(&m, id).await?;
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
    async fn hydrate_shard(&self, manifest: &VectorManifest, id: u16) -> Result<()> {
        let Some(hash) = manifest.entries.get(&id) else {
            for (key, _) in self.inner.collect_where(|k| shard_of(k, self.shards) == id) {
                self.inner.remove(&key).await?;
            }
            return Ok(());
        };
        let bytes = self.store.get_blob(hash).await?.ok_or_else(|| {
            CoreError::Backend(format!(
                "vector index {}: shard {id} names blob {} but it is absent",
                self.key, hash.0
            ))
        })?;
        let (dim, entries) = decode_shard(&bytes)?;
        if dim != self.dim {
            return Err(CoreError::Backend(format!(
                "vector index {}: shard {id} has dimension {dim}, manifest says {}",
                self.key, self.dim
            )));
        }
        for (key, _) in self.inner.collect_where(|k| shard_of(k, self.shards) == id) {
            self.inner.remove(&key).await?;
        }
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
    use gonzalo_core::{BlobStore, Identity, Meta, PutResult, Record, RecordKind, Store};
    use gonzalo_store_fs::FsStore;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        TempDir::new().unwrap()
    }

    /// A fresh handle onto the same directory. `FsStore` is not `Clone`, and a
    /// second handle is what a restart actually looks like anyway.
    fn fs(dir: &TempDir) -> FsStore {
        FsStore::new(dir.path())
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
}
