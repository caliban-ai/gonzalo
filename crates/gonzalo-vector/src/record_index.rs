//! A [`VectorIndex`] whose contents survive the process that built it (ADR 0027).
//!
//! Vectors live in content-addressed shard blobs named by one
//! [`VectorManifest`] record. Queries are served from an in-memory
//! [`MemoryVectorIndex`] hydrated at [`open`](RecordVectorIndex::open); writes
//! go through to the store under OCC.
//!
//! **One writer per index.** A concurrent writer is detected and retried, not
//! merged, and a second writer's in-memory view can be stale until it reopens.

use crate::shard::{DEFAULT_SHARDS, decode_shard, encode_shard, shard_of};
use crate::{Match, MemoryVectorIndex, VectorIndex};
use async_trait::async_trait;
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey,
    RecordKind, Result, Store, VectorManifest,
};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;
use std::sync::Arc;

/// How many shard blobs to read at once when opening. Matches
/// `LIST_READ_CONCURRENCY` in `gonzalo-store-s3`, which settled on 16 for the
/// same reason: enough parallelism to hide latency, not enough to thrash.
const SHARD_READ_CONCURRENCY: usize = 16;

/// How many times a commit re-reads and retries before giving up.
const MAX_COMMIT_ATTEMPTS: usize = 5;

/// One pending change to apply and persist.
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

/// Errors if `manifest`'s embedding space or dimension disagree with `space`
/// and `dim`, naming both values.
///
/// Shared between `open_with_shards` (checking an existing manifest at open)
/// and `commit`'s conflict arm (checking the winner's manifest, which this
/// handle may never have read at open — it could have opened a *missing*
/// index that another handle, declaring a different space, raced to create).
/// Without the second call site a swapped embedder only gets caught when both
/// handles happen to open onto an already-existing manifest; two handles that
/// both open a missing index skip the check entirely and mix spaces silently.
fn check_space_and_dim(
    key: &RecordKey,
    manifest: &VectorManifest,
    space: &str,
    dim: usize,
) -> Result<()> {
    if manifest.space != space {
        return Err(CoreError::Invalid(format!(
            "vector index {key}: stored embedding space is {:?}, opened as {:?}",
            manifest.space, space
        )));
    }
    if manifest.dim != dim {
        return Err(CoreError::Invalid(format!(
            "vector index {key}: stored dimension is {}, opened as {}",
            manifest.dim, dim
        )));
    }
    Ok(())
}

pub struct RecordVectorIndex<S> {
    store: Arc<S>,
    key: RecordKey,
    space: String,
    dim: usize,
    shards: NonZeroU16,
    inner: MemoryVectorIndex,
    /// The identity/origin stamped on manifest commits.
    meta: Meta,
    /// The manifest record this handle last observed committed — from `open`,
    /// from its own last successful commit, or from reloading the winner after
    /// a lost race. A commit's first attempt builds on this rather than a
    /// fresh store read, which is what makes a second writer's stale view a
    /// **real** OCC conflict: without it, every commit would re-read the
    /// current record right before writing and could never lose a race.
    last_seen: tokio::sync::Mutex<Option<Record>>,
    /// Serializes `commit` calls on this handle. `upsert`/`remove` take
    /// `&self`, so without this two calls on the same handle could interleave
    /// their reads of `last_seen` and each other's staged writes, including
    /// the `Committed` arm setting `last_seen` back to an older revision than
    /// a commit that finished first. Held for the whole of `commit`, never
    /// across a `std::sync` guard.
    commit_lock: tokio::sync::Mutex<()>,
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
                check_space_and_dim(&key, &m, space, dim)?;
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
            last_seen: tokio::sync::Mutex::new(existing),
            commit_lock: tokio::sync::Mutex::new(()),
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
    /// Used by `commit`'s conflict path to bring memory in line with a shard
    /// the winner changed since this handle last synced — one shard at a
    /// time, since a conflict only ever needs to catch up on the handful of
    /// shards that actually diverged, not the whole manifest the way
    /// `open_with_shards`'s batch concurrency does.
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

    /// What shard `id` would contain if `deltas` committed, computed from the
    /// *currently committed* in-memory state without mutating it.
    ///
    /// Memory only ever reflects a committed manifest (see `commit`), so this
    /// starts from what's already there for `id` and layers `deltas` on top of
    /// a copy — never on `self.inner` itself. That is what lets a failed
    /// commit (a `put_blob`/`put` error, or every attempt conflicting) leave
    /// the handle serving exactly what the store has, with nothing it never
    /// actually wrote.
    fn stage_shard(&self, id: u16, deltas: &[Delta]) -> Vec<(RecordKey, Vec<f32>)> {
        let mut staged: BTreeMap<RecordKey, Vec<f32>> = self
            .inner
            .collect_where(|k| shard_of(k, self.shards) == id)
            .into_iter()
            .collect();
        for delta in deltas {
            match delta {
                Delta::Upsert(key, vector) if shard_of(key, self.shards) == id => {
                    staged.insert(key.clone(), vector.clone());
                }
                Delta::Remove(key) if shard_of(key, self.shards) == id => {
                    staged.remove(key);
                }
                _ => {}
            }
        }
        staged.into_iter().collect()
    }

    /// Persist `deltas` as shard blobs plus an updated manifest, under OCC.
    ///
    /// The write is built from `self.last_seen` — what this handle last
    /// observed committed, not a fresh read of the store — so a second
    /// writer's stale view genuinely conflicts rather than quietly winning
    /// because nothing raced it in the same instant.
    ///
    /// Memory is never mutated ahead of a commit: each dirty shard is staged
    /// from the committed state already in memory (`stage_shard`), and
    /// `deltas` only land in `self.inner` once `store.put` reports
    /// `Committed`. That keeps memory consistent with the store even when a
    /// `put_blob`/`put` call errors or every attempt conflicts.
    ///
    /// On a conflict, every shard the winner changed *relative to what this
    /// handle last knew* — not just the shards this commit itself touches —
    /// is reloaded from the winner's blob into memory, because a shard this
    /// commit didn't touch but the winner did would otherwise stay stale in
    /// `self.inner` while `last_seen` claims memory is current; the next
    /// staged write from that shard would then silently drop the winner's
    /// vectors with no conflict to catch it. `dirty` is reloaded too even
    /// when unchanged, since it costs nothing and removes the shard from
    /// needing separate reasoning. This commit's own deltas are re-staged on
    /// top on retry, so a lost race costs a retry rather than either side's
    /// vectors. Shard blobs orphaned by a lost race are left for `gc`,
    /// exactly as the graph indexer does.
    ///
    /// `commit_lock` serializes this against any other `commit` on the same
    /// handle, so `last_seen` can't be read by one call and clobbered by
    /// another's `Committed` arm mid-flight.
    async fn commit(&self, deltas: Vec<Delta>) -> Result<()> {
        let _commit_guard = self.commit_lock.lock().await;

        let mut dirty = BTreeSet::new();
        for delta in &deltas {
            let key = match delta {
                Delta::Upsert(key, _) | Delta::Remove(key) => key,
            };
            dirty.insert(shard_of(key, self.shards));
        }

        for attempt in 1..=MAX_COMMIT_ATTEMPTS {
            let base = self.last_seen.lock().await.clone();
            let base_entries = match &base {
                Some(record) => VectorManifest::from_body(&record.body)?.entries,
                None => BTreeMap::new(),
            };

            let mut blobs = BTreeMap::new();
            for &id in &dirty {
                let staged = self.stage_shard(id, &deltas);
                let hash = self
                    .store
                    .put_blob(&encode_shard(self.dim, &staged))
                    .await?;
                blobs.insert(id, hash);
            }

            let mut manifest = match &base {
                Some(record) => VectorManifest::from_body(&record.body)?,
                None => VectorManifest::new(&self.space, self.dim, self.shards.get()),
            };
            for (id, hash) in &blobs {
                manifest.entries.insert(*id, hash.clone());
            }

            let body = manifest.to_body();
            let (record, expected) = match &base {
                Some(current) => (
                    current.update(body, self.meta.clone()),
                    Some(current.revision.clone()),
                ),
                None => (
                    Record::create(
                        self.key.clone(),
                        RecordKind::VectorManifest,
                        body,
                        self.meta.clone(),
                    ),
                    None,
                ),
            };

            match self.store.put(record.clone(), expected).await? {
                PutResult::Committed(rev) => {
                    self.apply(&deltas).await?;
                    // The store's returned revision is authoritative, not the
                    // one we locally built and sent — it can re-stamp it (for
                    // example, recreating a record over a tombstone). Caching
                    // our own guess here instead would make the next commit's
                    // `expected` mismatch the store for no real reason; it
                    // would recover on the resulting conflict, but there's no
                    // reason to pay for a conflict that isn't real.
                    let mut committed = record;
                    committed.revision = rev;
                    *self.last_seen.lock().await = Some(committed);
                    return Ok(());
                }
                PutResult::Conflict(_) if attempt < MAX_COMMIT_ATTEMPTS => {
                    // Someone else committed since we last synced with the
                    // store. Bring memory in line with every shard the winner
                    // changed relative to what we knew, plus every shard this
                    // commit touches, then retry with our own deltas staged
                    // back on top of the refreshed memory.
                    let winner = self.store.get(&self.key).await?.ok_or_else(|| {
                        CoreError::Backend(format!(
                            "vector index {}: manifest vanished mid-commit",
                            self.key
                        ))
                    })?;
                    let winner_manifest = VectorManifest::from_body(&winner.body)?;

                    // The winner may be a manifest this handle never actually
                    // checked: an `open` onto a *missing* index skips the
                    // space/dim/shards check entirely (there's nothing to
                    // check yet), so two handles can each open a missing
                    // index declaring different embedding spaces, dimensions,
                    // or shard counts and only discover the disagreement here,
                    // once one of them has already committed. A handle whose
                    // configuration disagrees with the committed index must
                    // stop rather than reload and write into it — reloading a
                    // shard under a different shard count would compute
                    // different shard ids for the same keys than the winner
                    // did, corrupting the manifest; writing a different space
                    // is exactly the silent cross-space mixing the space tag
                    // exists to prevent.
                    check_space_and_dim(&self.key, &winner_manifest, &self.space, self.dim)?;
                    if winner_manifest.shards != self.shards.get() {
                        return Err(CoreError::Invalid(format!(
                            "vector index {}: stored shard count is {}, opened as {}",
                            self.key,
                            winner_manifest.shards,
                            self.shards.get()
                        )));
                    }

                    let mut to_hydrate = dirty.clone();
                    let mut ids: BTreeSet<u16> = base_entries.keys().copied().collect();
                    ids.extend(winner_manifest.entries.keys().copied());
                    for id in ids {
                        if base_entries.get(&id) != winner_manifest.entries.get(&id) {
                            to_hydrate.insert(id);
                        }
                    }
                    // If a `hydrate_shard` call below fails partway through,
                    // memory ends up with some shards at the winner's state
                    // and others still stale, while `last_seen` (set only
                    // after the loop) stays at the old base. That's fine, not
                    // a bug: the next commit will still use that same old
                    // base, so it's guaranteed to conflict again and re-diff
                    // against whatever the current manifest is at that point,
                    // which recomputes the full set of shards that actually
                    // differ — including the ones this attempt already
                    // brought up to date, re-hydrated harmlessly.
                    for id in to_hydrate {
                        self.hydrate_shard(&winner_manifest, id).await?;
                    }
                    *self.last_seen.lock().await = Some(winner);
                }
                PutResult::Conflict(_) => {
                    return Err(CoreError::Backend(format!(
                        "vector index {}: gave up after {MAX_COMMIT_ATTEMPTS} conflicting commits",
                        self.key
                    )));
                }
            }
        }
        unreachable!("the loop returns on the final attempt")
    }

    /// Apply deltas to the in-memory index only. Called only after a commit
    /// that produced them has actually landed in the store — see `commit`.
    async fn apply(&self, deltas: &[Delta]) -> Result<()> {
        for delta in deltas {
            match delta {
                Delta::Upsert(key, vector) => {
                    self.inner.upsert(key.clone(), vector.clone()).await?
                }
                Delta::Remove(key) => self.inner.remove(key).await?,
            }
        }
        Ok(())
    }
}

#[async_trait]
impl<S: Store + BlobStore + Send + Sync + 'static> VectorIndex for RecordVectorIndex<S> {
    async fn upsert(&self, key: RecordKey, vector: Vec<f32>) -> Result<()> {
        if vector.len() != self.dim {
            return Err(CoreError::Invalid(format!(
                "vector index {}: expected dimension {}, got {}",
                self.key,
                self.dim,
                vector.len()
            )));
        }
        self.commit(vec![Delta::Upsert(key, vector)]).await
    }

    async fn remove(&self, key: &RecordKey) -> Result<()> {
        self.commit(vec![Delta::Remove(key.clone())]).await
    }

    /// Overrides the trait's looping default: one batch becomes one `commit`
    /// call, so a bulk load stages every shard once and writes one manifest
    /// revision instead of one per vector. Dimensions are validated up front,
    /// before anything is staged, so a bad batch fails without touching the
    /// store at all.
    async fn upsert_many(&self, items: Vec<(RecordKey, Vec<f32>)>) -> Result<()> {
        for (key, vector) in &items {
            if vector.len() != self.dim {
                return Err(CoreError::Invalid(format!(
                    "vector index {}: {key} has dimension {}, index is {}",
                    self.key,
                    vector.len(),
                    self.dim
                )));
            }
        }
        self.commit(
            items
                .into_iter()
                .map(|(key, vector)| Delta::Upsert(key, vector))
                .collect(),
        )
        .await
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

    /// Wraps a real `FsStore` but fails every `put_blob`, so a commit's
    /// shard-write step always errors. Used to prove a failed commit leaves
    /// memory exactly matching the store: deltas must not have already landed
    /// in `self.inner` when the write meant to persist them never lands.
    struct FailingBlobStore {
        inner: FsStore,
    }

    #[async_trait]
    impl Store for FailingBlobStore {
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
    impl BlobStore for FailingBlobStore {
        async fn put_blob(&self, _content: &[u8]) -> Result<ContentHash> {
            Err(CoreError::Backend("simulated blob store failure".into()))
        }

        async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
            self.inner.get_blob(hash).await
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

    // The headline claim of the whole ticket.
    #[tokio::test]
    async fn vectors_survive_reopening_the_index() {
        let dir = tmp();
        let s = fs(&dir);
        let k = RecordKey::new("ns", "coll", "a");

        let idx = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3)
            .await
            .unwrap();
        idx.upsert(k.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        drop(idx);

        let reopened = RecordVectorIndex::open(s, index_key(), "space-a", 3)
            .await
            .unwrap();
        let hits = reopened
            .query(&[1.0, 0.0, 0.0], 5, &KeyPrefix::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, k);
    }

    #[tokio::test]
    async fn a_removed_vector_stays_removed_after_reopening() {
        let dir = tmp();
        let s = fs(&dir);
        let k = RecordKey::new("ns", "coll", "a");

        let idx = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3)
            .await
            .unwrap();
        idx.upsert(k.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        idx.remove(&k).await.unwrap();
        drop(idx);

        let reopened = RecordVectorIndex::open(s, index_key(), "space-a", 3)
            .await
            .unwrap();
        assert!(
            reopened
                .keys(&KeyPrefix::default())
                .await
                .unwrap()
                .is_empty()
        );
    }

    // Two handles on one index is the case OCC exists for: the loser must not
    // drop the winner's vector, nor its own.
    //
    // Forced to a single shard: with the default 256 shards `a` (shard 121)
    // and `b` (shard 136) never contend for the same shard, so this can pass
    // without the conflict path ever running at all — it would only be
    // proving that two non-overlapping writes don't clobber each other, which
    // is true of a design with no OCC whatsoever. Pinning both writers to one
    // shard forces the actual collision.
    #[tokio::test]
    async fn a_concurrent_writer_loses_neither_sides_vectors() {
        let dir = tmp();
        let s = fs(&dir);
        let a = RecordKey::new("ns", "coll", "a");
        let b = RecordKey::new("ns", "coll", "b");
        let one_shard = std::num::NonZeroU16::new(1).unwrap();

        let one =
            RecordVectorIndex::open_with_shards(fs(&dir), index_key(), "space-a", 3, one_shard)
                .await
                .unwrap();
        let two =
            RecordVectorIndex::open_with_shards(fs(&dir), index_key(), "space-a", 3, one_shard)
                .await
                .unwrap();

        one.upsert(a.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        // `two` opened before that commit, so its manifest read is stale and
        // this commit must conflict, reload, and retry.
        two.upsert(b.clone(), vec![0.0, 1.0, 0.0]).await.unwrap();

        let reopened = RecordVectorIndex::open(s, index_key(), "space-a", 3)
            .await
            .unwrap();
        let mut got = reopened.keys(&KeyPrefix::default()).await.unwrap();
        got.sort();
        assert_eq!(got, vec![a, b]);
    }

    // A lost race leaves the loser's shard blob unreferenced. Nothing points at
    // it, so gc must reclaim it — and must not touch the shard that won.
    //
    // Forced to a single shard: with the default 256 shards `a` and `b` land in
    // different buckets, and two non-overlapping single-vector shard writes
    // never collide — each blob is exactly what the final manifest ends up
    // referencing, so nothing is ever orphaned. Pinning both writers to one
    // shard guarantees they contend for the same blob, which is the only way
    // a lost race actually leaves one behind.
    #[tokio::test]
    async fn a_shard_blob_orphaned_by_a_lost_race_is_reclaimed() {
        let dir = tmp();
        let s = fs(&dir);
        let a = RecordKey::new("ns", "coll", "a");
        let b = RecordKey::new("ns", "coll", "b");
        let one_shard = std::num::NonZeroU16::new(1).unwrap();

        let one =
            RecordVectorIndex::open_with_shards(fs(&dir), index_key(), "space-a", 3, one_shard)
                .await
                .unwrap();
        let two =
            RecordVectorIndex::open_with_shards(fs(&dir), index_key(), "space-a", 3, one_shard)
                .await
                .unwrap();
        one.upsert(a.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        two.upsert(b.clone(), vec![0.0, 1.0, 0.0]).await.unwrap();

        let before = s.list_blobs().await.unwrap().len();
        let report = gonzalo_core::gc::gc_blobs(&s).await.unwrap();
        let after = s.list_blobs().await.unwrap().len();

        // `GcReport.freed` is a Vec<ContentHash> of what was deleted, not a count
        // (crates/gonzalo-core/src/gc.rs:18).
        assert!(
            !report.freed.is_empty(),
            "the orphaned shard blob should be swept"
        );
        assert!(after < before);

        // The surviving index must still be intact afterwards, holding both
        // vectors — not just `b`. Under a design that reloads only the shards
        // a commit itself touches (rather than every shard the winner
        // changed), the winner's own vector is exactly what a lost race can
        // silently drop, and asserting only `b` survives would miss that.
        let reopened = RecordVectorIndex::open(s, index_key(), "space-a", 3)
            .await
            .unwrap();
        let mut got = reopened.keys(&KeyPrefix::default()).await.unwrap();
        got.sort();
        assert_eq!(got, vec![a, b.clone()]);
        let hits = reopened
            .query(&[0.0, 1.0, 0.0], 1, &KeyPrefix::default())
            .await
            .unwrap();
        assert_eq!(hits[0].key, b);
    }

    // Regression: a conflict reload that only catches up on the shards *this*
    // commit touches is not enough. Fixed 256-shard keys, chosen so `a` and
    // `c661` land in the same shard (121) while `b` lands in a different one
    // (136) -- verified directly against `shard_of`, not assumed.
    //
    //   1. `one` and `two` both open the empty index (R0).
    //   2. `one` commits `a` alone -- shard 121, manifest R1.
    //   3. `two`'s commit of `b` (shard 136) conflicts against its stale R0
    //      view. A reload that only refreshes `dirty` (136) leaves `two`'s
    //      memory for shard 121 empty even though the winner (R1) already has
    //      `a` there; a reload keyed off every shard the winner *changed*
    //      catches shard 121 too.
    //   4. `two` then commits `c661`, also shard 121. If step 3 left shard
    //      121 stale (empty) in `two`'s memory, this re-serialises shard 121
    //      as `{c661}` alone -- silently dropping `a` -- and commits clean,
    //      because `two`'s cached revision (R2) is already current: no
    //      conflict occurs to catch it.
    #[tokio::test]
    async fn a_conflict_reload_catches_up_shards_the_commit_itself_did_not_touch() {
        let dir = tmp();
        let s = fs(&dir);
        let a = RecordKey::new("ns", "coll", "a");
        let b = RecordKey::new("ns", "coll", "b");
        let c = RecordKey::new("ns", "coll", "c661");
        assert_eq!(shard_of(&a, DEFAULT_SHARDS), shard_of(&c, DEFAULT_SHARDS));
        assert_ne!(shard_of(&a, DEFAULT_SHARDS), shard_of(&b, DEFAULT_SHARDS));

        let one = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3)
            .await
            .unwrap();
        let two = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3)
            .await
            .unwrap();

        one.upsert(a.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        two.upsert(b.clone(), vec![0.0, 1.0, 0.0]).await.unwrap();
        two.upsert(c.clone(), vec![0.0, 0.0, 1.0]).await.unwrap();

        let reopened = RecordVectorIndex::open(s, index_key(), "space-a", 3)
            .await
            .unwrap();
        let mut got = reopened.keys(&KeyPrefix::default()).await.unwrap();
        got.sort();
        let mut want = vec![a, b, c];
        want.sort();
        assert_eq!(
            got, want,
            "a lost race must not silently drop a shard the losing commit didn't itself touch"
        );
    }

    // A failed commit (here, every `put_blob` errors) must leave the handle's
    // in-memory index exactly matching what the store actually has -- empty,
    // in this case -- never serving a vector that was never persisted.
    #[tokio::test]
    async fn a_failed_commit_leaves_memory_matching_the_store() {
        let dir = tmp();
        let s = FailingBlobStore { inner: fs(&dir) };
        let idx = RecordVectorIndex::open(s, index_key(), "space-a", 3)
            .await
            .unwrap();
        let k = RecordKey::new("ns", "coll", "a");

        let result = idx.upsert(k, vec![1.0, 0.0, 0.0]).await;
        assert!(
            result.is_err(),
            "a failing put_blob must surface as an error"
        );
        assert!(
            idx.keys(&KeyPrefix::default()).await.unwrap().is_empty(),
            "a failed commit must not leave an uncommitted vector visible in memory"
        );
    }

    // Two handles can each `open` a *missing* index -- there's no manifest yet
    // to check `space`/`dim` against, so the check in `open_with_shards` never
    // runs for either of them. If the conflict path doesn't check the winner's
    // manifest either, the second handle's commit reloads and writes right
    // into an index declaring a different embedding space, with nothing to
    // catch it (dimensions match, so even the in-memory dimension check is
    // silent).
    #[tokio::test]
    async fn a_conflicting_commit_refuses_to_join_a_different_embedding_space() {
        let dir = tmp();
        let s = fs(&dir);
        let a = RecordKey::new("ns", "coll", "a");
        let b = RecordKey::new("ns", "coll", "b");

        let one = RecordVectorIndex::open(fs(&dir), index_key(), "model-a", 3)
            .await
            .unwrap();
        let two = RecordVectorIndex::open(fs(&dir), index_key(), "model-b", 3)
            .await
            .unwrap();

        one.upsert(a.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        // `two` opened before that commit, so it conflicts -- and only then
        // discovers the space disagreement.
        let err = two
            .upsert(b, vec![0.0, 1.0, 0.0])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("model-a"),
            "winner's space missing from: {err}"
        );
        assert!(
            err.contains("model-b"),
            "this handle's space missing from: {err}"
        );

        // The refused write must not have landed: only `one`'s vector exists.
        let reopened = RecordVectorIndex::open(s, index_key(), "model-a", 3)
            .await
            .unwrap();
        assert_eq!(reopened.keys(&KeyPrefix::default()).await.unwrap(), vec![a]);
    }

    // Same hole, for shard count: two `open_with_shards` handles that both
    // open a missing index each pick their own shard count, since there's no
    // existing manifest to adopt one from. If a conflicting commit doesn't
    // check the winner's shard count before reloading, it would compute shard
    // ids for its own keys under its *own* shard count while the winner's
    // blobs are addressed under a different one -- corrupting the manifest.
    #[tokio::test]
    async fn a_conflicting_commit_refuses_to_join_a_different_shard_count() {
        let dir = tmp();
        let s = fs(&dir);
        let a = RecordKey::new("ns", "coll", "a");
        let b = RecordKey::new("ns", "coll", "b");

        let one = RecordVectorIndex::open_with_shards(
            fs(&dir),
            index_key(),
            "space-a",
            3,
            std::num::NonZeroU16::new(4).unwrap(),
        )
        .await
        .unwrap();
        let two = RecordVectorIndex::open_with_shards(
            fs(&dir),
            index_key(),
            "space-a",
            3,
            std::num::NonZeroU16::new(8).unwrap(),
        )
        .await
        .unwrap();

        one.upsert(a.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        let err = two
            .upsert(b, vec![0.0, 1.0, 0.0])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains('4'),
            "winner's shard count missing from: {err}"
        );
        assert!(
            err.contains('8'),
            "this handle's shard count missing from: {err}"
        );

        // The refused write must not have landed: only `one`'s vector exists.
        // Reopening with `open` adopts the stored shard count (4).
        let reopened = RecordVectorIndex::open(s, index_key(), "space-a", 3)
            .await
            .unwrap();
        assert_eq!(reopened.keys(&KeyPrefix::default()).await.unwrap(), vec![a]);
    }

    #[tokio::test]
    async fn rewriting_identical_content_reuses_the_same_blob() {
        let dir = tmp();
        let s = fs(&dir);
        let k = RecordKey::new("ns", "coll", "a");

        let idx = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3)
            .await
            .unwrap();
        idx.upsert(k.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        let first = s.get(&index_key()).await.unwrap().unwrap();

        idx.upsert(k, vec![1.0, 0.0, 0.0]).await.unwrap();
        let second = s.get(&index_key()).await.unwrap().unwrap();

        let a = VectorManifest::from_body(&first.body).unwrap();
        let b = VectorManifest::from_body(&second.body).unwrap();
        assert_eq!(
            a.entries, b.entries,
            "identical content should hash the same"
        );
    }

    // Committing once per vector would mean one manifest commit per vector, so a
    // bulk load of 100k chunks would be 100k commits. One batch, one revision.
    #[tokio::test]
    async fn upsert_many_commits_the_manifest_once() {
        let dir = tmp();
        let s = fs(&dir);
        let idx = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3)
            .await
            .unwrap();

        idx.upsert_many(
            (0..64)
                .map(|i| {
                    (
                        RecordKey::new("ns", "coll", i.to_string()),
                        vec![i as f32, 0.0, 0.0],
                    )
                })
                .collect(),
        )
        .await
        .unwrap();

        let record = s.get(&index_key()).await.unwrap().unwrap();
        // `Record::create` stamps `Revision::initial`, whose counter is 0 (see
        // `gonzalo_core::revision::Revision::initial`), so the *first* commit
        // on a brand-new manifest lands at counter 0, not 1. What matters
        // here is that there is exactly one commit for 64 upserts: a looping
        // default would leave the counter at 63 (63 updates on top of the
        // initial create), not 0.
        assert_eq!(record.revision.counter, 0, "one batch must be one revision");
        assert_eq!(idx.keys(&KeyPrefix::default()).await.unwrap().len(), 64);
    }

    // The `Arc<T>` delegation impl forwards `upsert_many` explicitly rather
    // than inheriting the default -- but `MemoryVectorIndex` (the only other
    // implementor) never overrides `upsert_many`, so before this override
    // existed, forwarding and inheriting the trait default were behaviourally
    // identical: both would loop over `upsert`. This is the first case where
    // they diverge, so it's the only test that can actually catch someone
    // later dropping the explicit forward from the `Arc` impl (which would
    // silently fall back to the looping default and defeat the batching this
    // task adds).
    #[tokio::test]
    async fn upsert_many_through_an_arc_still_commits_the_manifest_once() {
        let dir = tmp();
        let s = fs(&dir);
        let idx: std::sync::Arc<RecordVectorIndex<FsStore>> = std::sync::Arc::new(
            RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3)
                .await
                .unwrap(),
        );

        idx.upsert_many(
            (0..64)
                .map(|i| {
                    (
                        RecordKey::new("ns", "coll", i.to_string()),
                        vec![i as f32, 0.0, 0.0],
                    )
                })
                .collect(),
        )
        .await
        .unwrap();

        let record = s.get(&index_key()).await.unwrap().unwrap();
        // See `upsert_many_commits_the_manifest_once`: the first commit on a
        // fresh manifest lands at counter 0 (`Revision::initial`), not 1.
        assert_eq!(
            record.revision.counter, 0,
            "one batch through Arc<RecordVectorIndex<_>> must still be one revision"
        );
        assert_eq!(idx.keys(&KeyPrefix::default()).await.unwrap().len(), 64);
    }
}
