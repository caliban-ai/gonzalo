//! Approximate ANN vector index backed by `hnsw_rs` (ADR 0014).
//!
//! `hnsw_rs` keys by `usize` and supports neither deletion nor in-place update,
//! so [`HnswVectorIndex`] owns a small layer over it: a `RecordKey ↔ usize`
//! bimap, a tombstone set for removed/replaced ids, and a bounded rebuild that
//! reclaims tombstoned graph space once the dead entries outnumber the live
//! ones. Scores are exact cosine, recomputed on the candidates the graph
//! returns; the [`KeyPrefix`] filter is applied by post-filtering those
//! candidates. Because retrieval is approximate and tombstone/filter-reduced,
//! [`query`](HnswVectorIndex::query) may return fewer than `k` matches.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use gonzalo_core::{CoreError, KeyPrefix, RecordKey, Result};
use hnsw_rs::prelude::{DistCosine, Hnsw};

use crate::index::cosine;
use crate::{Match, VectorIndex};

// hnsw construction parameters — sensible defaults for CPU in-memory use.
const MAX_NB_CONNECTION: usize = 16;
const MAX_LAYER: usize = 16;
const EF_CONSTRUCTION: usize = 200;
const INITIAL_CAPACITY: usize = 10_000;
/// Minimum `ef` for search (candidate breadth); larger trades latency for recall.
const EF_SEARCH_MIN: usize = 64;
/// Over-fetch factor so post-filtering by tombstones/`KeyPrefix` still yields `k`.
const OVERFETCH: usize = 4;
/// Don't rebuild for churn below this many tombstones (avoid thrashing).
const MIN_REBUILD_TOMBSTONES: usize = 64;

struct Inner {
    hnsw: Hnsw<'static, f32, DistCosine>,
    next_id: usize,
    key_to_id: HashMap<RecordKey, usize>,
    id_to_entry: HashMap<usize, (RecordKey, Vec<f32>)>,
    dim: Option<usize>,
    tombstones: usize,
    capacity: usize,
}

impl Inner {
    fn new_graph(capacity: usize) -> Hnsw<'static, f32, DistCosine> {
        Hnsw::new(
            MAX_NB_CONNECTION,
            capacity,
            MAX_LAYER,
            EF_CONSTRUCTION,
            DistCosine,
        )
    }

    fn empty() -> Self {
        Self {
            hnsw: Self::new_graph(INITIAL_CAPACITY),
            next_id: 0,
            key_to_id: HashMap::new(),
            id_to_entry: HashMap::new(),
            dim: None,
            tombstones: 0,
            capacity: INITIAL_CAPACITY,
        }
    }

    fn check_dim(&self, incoming: usize) -> Result<()> {
        if let Some(expected) = self.dim
            && incoming != expected
        {
            return Err(CoreError::Backend(format!(
                "vector dimension mismatch: expected {expected}, got {incoming}"
            )));
        }
        Ok(())
    }

    /// Insert a live entry under a fresh id.
    fn insert_live(&mut self, key: RecordKey, vector: Vec<f32>) {
        let id = self.next_id;
        self.next_id += 1;
        self.hnsw.insert((&vector, id));
        self.key_to_id.insert(key.clone(), id);
        self.id_to_entry.insert(id, (key, vector));
    }

    /// Rebuild the graph from the live entries when tombstones dominate, or when
    /// the id space approaches the graph's capacity.
    fn maybe_rebuild(&mut self) {
        let live = self.id_to_entry.len();
        let dominated = self.tombstones > live && self.tombstones > MIN_REBUILD_TOMBSTONES;
        if dominated || self.next_id >= self.capacity {
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        let entries: Vec<(RecordKey, Vec<f32>)> =
            self.id_to_entry.drain().map(|(_, entry)| entry).collect();
        let capacity = (entries.len() * 2).max(INITIAL_CAPACITY);
        self.hnsw = Self::new_graph(capacity);
        self.capacity = capacity;
        self.next_id = 0;
        self.key_to_id.clear();
        self.tombstones = 0;
        for (key, vector) in entries {
            self.insert_live(key, vector);
        }
    }
}

/// An approximate, in-memory ANN vector index keyed by [`RecordKey`].
///
/// A scalable alternative to [`MemoryVectorIndex`](crate::MemoryVectorIndex):
/// query is sub-linear (HNSW) rather than brute-force, at the cost of
/// approximate recall. `MemoryVectorIndex` remains the exact default.
pub struct HnswVectorIndex {
    inner: Mutex<Inner>,
}

impl HnswVectorIndex {
    /// Create an empty index.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::empty()),
        }
    }

    #[cfg(test)]
    /// `(next_id, live, tombstones)` — for tests asserting rebuild behavior.
    fn debug_state(&self) -> (usize, usize, usize) {
        let inner = self.inner.lock().expect("mutex poisoned");
        (inner.next_id, inner.id_to_entry.len(), inner.tombstones)
    }
}

impl Default for HnswVectorIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VectorIndex for HnswVectorIndex {
    async fn upsert(&self, key: RecordKey, vector: Vec<f32>) -> Result<()> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        inner.check_dim(vector.len())?;
        if inner.dim.is_none() {
            inner.dim = Some(vector.len());
        }
        // Replacing a key tombstones its old id (the graph has no update).
        if let Some(old_id) = inner.key_to_id.remove(&key) {
            inner.id_to_entry.remove(&old_id);
            inner.tombstones += 1;
        }
        inner.insert_live(key, vector);
        inner.maybe_rebuild();
        Ok(())
    }

    async fn remove(&self, key: &RecordKey) -> Result<()> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        if let Some(id) = inner.key_to_id.remove(key) {
            inner.id_to_entry.remove(&id);
            inner.tombstones += 1;
            inner.maybe_rebuild();
        }
        Ok(())
    }

    async fn query(&self, query: &[f32], k: usize, filter: &KeyPrefix) -> Result<Vec<Match>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let inner = self.inner.lock().expect("mutex poisoned");
        if inner.id_to_entry.is_empty() {
            return Ok(Vec::new());
        }
        inner.check_dim(query.len())?;

        // Over-fetch so tombstoned ids and out-of-filter keys can be dropped
        // while still (usually) yielding k live, in-filter matches.
        let live = inner.id_to_entry.len();
        let knbn = k
            .saturating_mul(OVERFETCH)
            .saturating_add(inner.tombstones)
            .min(live + inner.tombstones)
            .max(k);
        let ef = knbn.max(EF_SEARCH_MIN);

        let mut matches: Vec<Match> = inner
            .hnsw
            .search(query, knbn, ef)
            .iter()
            // Skip tombstoned ids (absent from id_to_entry).
            .filter_map(|n| inner.id_to_entry.get(&n.d_id))
            .filter(|(key, _)| filter.matches(key))
            .map(|(key, vec)| Match {
                key: key.clone(),
                score: cosine(query, vec),
            })
            .collect();

        // Exact re-scoring means ordering matches MemoryVectorIndex: descending
        // score, ties broken by RecordKey (ascending).
        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.key.cmp(&b.key))
        });
        matches.truncate(k);
        Ok(matches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> RecordKey {
        RecordKey::new("ns", "col", id)
    }

    #[tokio::test]
    async fn query_returns_nearest_first() {
        let idx = HnswVectorIndex::new();
        idx.upsert(key("east"), vec![1.0, 0.0]).await.unwrap();
        idx.upsert(key("north"), vec![0.0, 1.0]).await.unwrap();
        idx.upsert(key("neg"), vec![-1.0, 0.0]).await.unwrap();

        let results = idx
            .query(&[0.99, 0.01], 3, &KeyPrefix::default())
            .await
            .unwrap();
        assert_eq!(results[0].key.id, "east");
    }

    #[tokio::test]
    async fn k_limits_and_oversized_k() {
        let idx = HnswVectorIndex::new();
        for i in 1..=5u8 {
            idx.upsert(key(&i.to_string()), vec![i as f32, 0.0])
                .await
                .unwrap();
        }
        let q = vec![1.0f32, 0.0];
        // `k` caps results; oversized `k` can't exceed the corpus. Bounds (not
        // exact counts) because the index is approximate — recall on a tiny,
        // randomly-constructed graph is not guaranteed to be total.
        let limited = idx.query(&q, 2, &KeyPrefix::default()).await.unwrap().len();
        assert!((1..=2).contains(&limited), "k=2 capped, got {limited}");
        let all = idx
            .query(&q, 100, &KeyPrefix::default())
            .await
            .unwrap()
            .len();
        assert!((1..=5).contains(&all), "k=100 bounded by corpus, got {all}");
    }

    #[tokio::test]
    async fn filter_restricts_to_namespace() {
        let idx = HnswVectorIndex::new();
        idx.upsert(RecordKey::new("alpha", "col", "a1"), vec![1.0, 0.0])
            .await
            .unwrap();
        idx.upsert(RecordKey::new("alpha", "col", "a2"), vec![1.0, 0.1])
            .await
            .unwrap();
        idx.upsert(RecordKey::new("beta", "col", "b1"), vec![0.0, 1.0])
            .await
            .unwrap();
        let filter = KeyPrefix {
            namespace: Some("alpha".into()),
            collection: None,
        };
        let results = idx.query(&[1.0, 0.0], 10, &filter).await.unwrap();
        // The filter is what's under test: every hit is in `alpha`, `beta` never
        // leaks. Assert filter-correctness + non-empty rather than an exact count
        // (the index is approximate).
        assert!(!results.is_empty());
        assert!(results.iter().all(|m| m.key.namespace == "alpha"));
    }

    #[tokio::test]
    async fn dimension_mismatch_is_error() {
        let idx = HnswVectorIndex::new();
        idx.upsert(key("a"), vec![1.0, 0.0]).await.unwrap();

        let up = idx.upsert(key("b"), vec![1.0, 0.0, 0.0]).await.unwrap_err();
        assert!(matches!(up, CoreError::Backend(ref m) if m.contains("dimension mismatch")));

        let q = idx
            .query(&[1.0, 0.0, 0.0], 1, &KeyPrefix::default())
            .await
            .unwrap_err();
        assert!(matches!(q, CoreError::Backend(ref m) if m.contains("dimension mismatch")));
    }

    #[tokio::test]
    async fn remove_drops_key_and_absent_is_ok() {
        let idx = HnswVectorIndex::new();
        idx.upsert(key("target"), vec![1.0, 0.0]).await.unwrap();
        idx.upsert(key("other"), vec![0.0, 1.0]).await.unwrap();

        idx.remove(&key("target")).await.unwrap();
        let results = idx
            .query(&[1.0, 0.0], 10, &KeyPrefix::default())
            .await
            .unwrap();
        assert!(results.iter().all(|m| m.key.id != "target"));

        // Removing an absent key is a no-op success.
        assert!(idx.remove(&key("ghost")).await.is_ok());
    }

    #[tokio::test]
    async fn reupsert_reflects_new_vector() {
        let idx = HnswVectorIndex::new();
        idx.upsert(key("x"), vec![1.0, 0.0]).await.unwrap();
        // Re-point x to a clearly different direction.
        idx.upsert(key("x"), vec![0.0, 1.0]).await.unwrap();
        idx.upsert(key("east"), vec![1.0, 0.0]).await.unwrap();

        // A north query should now rank x first (its new direction), not east.
        let results = idx
            .query(&[0.0, 1.0], 1, &KeyPrefix::default())
            .await
            .unwrap();
        assert_eq!(results[0].key.id, "x");
        // Exactly one live entry for x.
        let (_, live, _) = idx.debug_state();
        assert_eq!(live, 2, "x + east, the old x tombstoned");
    }

    #[tokio::test]
    async fn rebuild_reclaims_tombstones_and_preserves_entries() {
        let idx = HnswVectorIndex::new();
        // One survivor, then upsert+remove churn to cross MIN_REBUILD_TOMBSTONES
        // while live stays at 1 — the final remove trips the rebuild exactly.
        idx.upsert(key("keep"), vec![1.0, 0.0]).await.unwrap();
        for i in 0..=MIN_REBUILD_TOMBSTONES {
            let k = key(&format!("churn{i}"));
            idx.upsert(k.clone(), vec![0.0, 1.0]).await.unwrap();
            idx.remove(&k).await.unwrap();
        }

        // A rebuild has fired: tombstones reset, ids compacted to the live set.
        let (next_id, live, tombstones) = idx.debug_state();
        assert_eq!(tombstones, 0, "rebuild cleared tombstones");
        assert!(
            next_id <= live,
            "ids compacted to the live set: {next_id} <= {live}"
        );

        // The survivor is still retrievable and the churned keys stay gone.
        let results = idx
            .query(&[1.0, 0.0], 10, &KeyPrefix::default())
            .await
            .unwrap();
        assert!(results.iter().any(|m| m.key.id == "keep"));
        assert!(results.iter().all(|m| !m.key.id.starts_with("churn")));
    }
}
