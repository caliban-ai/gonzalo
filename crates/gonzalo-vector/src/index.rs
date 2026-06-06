//! In-memory exact cosine vector index keyed by [`RecordKey`].

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use gonzalo_core::{CoreError, KeyPrefix, RecordKey, Result};

use crate::{Match, VectorIndex};

// ---------------------------------------------------------------------------
// Cosine helper
// ---------------------------------------------------------------------------

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

// ---------------------------------------------------------------------------
// MemoryVectorIndex
// ---------------------------------------------------------------------------

/// An exact, brute-force in-memory vector index.
///
/// All vectors must share the same dimension once the first entry is inserted.
/// Cosine similarity is used for scoring; results are ordered by descending
/// score with ties broken by [`RecordKey`] ordering (lexicographic), making
/// results deterministic.
#[derive(Default)]
pub struct MemoryVectorIndex {
    store: Mutex<HashMap<RecordKey, Vec<f32>>>,
}

impl MemoryVectorIndex {
    /// Create an empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the current dimension of stored vectors, or `None` if the index
    /// is empty.
    fn stored_dim(map: &HashMap<RecordKey, Vec<f32>>) -> Option<usize> {
        map.values().next().map(|v| v.len())
    }

    fn check_dim(map: &HashMap<RecordKey, Vec<f32>>, incoming: usize) -> Result<()> {
        if let Some(expected) = Self::stored_dim(map)
            && incoming != expected
        {
            return Err(CoreError::Backend(format!(
                "vector dimension mismatch: expected {expected}, got {incoming}"
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl VectorIndex for MemoryVectorIndex {
    async fn upsert(&self, key: RecordKey, vector: Vec<f32>) -> Result<()> {
        let mut map = self.store.lock().expect("mutex poisoned");
        Self::check_dim(&map, vector.len())?;
        map.insert(key, vector);
        Ok(())
    }

    async fn remove(&self, key: &RecordKey) -> Result<()> {
        let mut map = self.store.lock().expect("mutex poisoned");
        map.remove(key);
        Ok(())
    }

    async fn query(&self, query: &[f32], k: usize, filter: &KeyPrefix) -> Result<Vec<Match>> {
        let map = self.store.lock().expect("mutex poisoned");
        if !map.is_empty() {
            Self::check_dim(&map, query.len())?;
        }

        let mut matches: Vec<Match> = map
            .iter()
            .filter(|(key, _)| filter.matches(key))
            .map(|(key, vec)| Match {
                key: key.clone(),
                score: cosine(query, vec),
            })
            .collect();

        // Sort descending by score; break ties by RecordKey order (ascending).
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Embedder;

    /// Tiny deterministic embedder: maps a string to a 4-element Vec<f32> by
    /// bucketing each byte value into one of four bins and summing them.
    struct BucketEmbedder;

    #[async_trait]
    impl Embedder for BucketEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let mut out = vec![0.0f32; 4];
            for (i, b) in text.bytes().enumerate() {
                out[i % 4] += b as f32;
            }
            Ok(out)
        }
    }

    // ------------------------------------------------------------------
    // Test 1: nearest-first ordering
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn query_returns_nearest_first() {
        let idx = MemoryVectorIndex::new();

        // Three 2-D vectors pointing in clearly different directions.
        idx.upsert(RecordKey::new("ns", "col", "east"), vec![1.0, 0.0])
            .await
            .unwrap();
        idx.upsert(RecordKey::new("ns", "col", "north"), vec![0.0, 1.0])
            .await
            .unwrap();
        idx.upsert(RecordKey::new("ns", "col", "neg"), vec![-1.0, 0.0])
            .await
            .unwrap();

        // Query pointing almost east.
        let results = idx
            .query(&[0.99, 0.01], 3, &KeyPrefix::default())
            .await
            .unwrap();

        assert_eq!(results[0].key.id, "east");
    }

    // ------------------------------------------------------------------
    // Test 2: k limits results; k > index size returns all
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn k_limits_and_oversized_k() {
        let idx = MemoryVectorIndex::new();
        for i in 0..5u8 {
            let v = vec![i as f32, 0.0];
            idx.upsert(RecordKey::new("ns", "col", format!("{i}")), v)
                .await
                .unwrap();
        }

        // Non-zero query so we don't divide by zero on the [0.0, 0.0] vector.
        let q = vec![1.0f32, 0.0];

        let limited = idx.query(&q, 2, &KeyPrefix::default()).await.unwrap();
        assert_eq!(limited.len(), 2);

        let all = idx.query(&q, 100, &KeyPrefix::default()).await.unwrap();
        assert_eq!(all.len(), 5);
    }

    // ------------------------------------------------------------------
    // Test 3: filter by namespace
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn filter_restricts_to_namespace() {
        let idx = MemoryVectorIndex::new();

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

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|m| m.key.namespace == "alpha"));
    }

    // ------------------------------------------------------------------
    // Test 4: dimension mismatch returns Backend error on upsert
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn upsert_dimension_mismatch_is_error() {
        let idx = MemoryVectorIndex::new();
        idx.upsert(RecordKey::new("ns", "col", "a"), vec![1.0, 0.0])
            .await
            .unwrap();

        let err = idx
            .upsert(RecordKey::new("ns", "col", "b"), vec![1.0, 0.0, 0.0])
            .await
            .unwrap_err();

        assert!(
            matches!(err, CoreError::Backend(ref msg) if msg.contains("dimension mismatch")),
            "unexpected error: {err}"
        );
    }

    // ------------------------------------------------------------------
    // Test 5: remove drops key from results
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn remove_drops_key() {
        let idx = MemoryVectorIndex::new();
        let key = RecordKey::new("ns", "col", "target");
        idx.upsert(key.clone(), vec![1.0, 0.0]).await.unwrap();
        idx.upsert(RecordKey::new("ns", "col", "other"), vec![0.0, 1.0])
            .await
            .unwrap();

        idx.remove(&key).await.unwrap();

        let results = idx
            .query(&[1.0, 0.0], 10, &KeyPrefix::default())
            .await
            .unwrap();
        assert!(results.iter().all(|m| m.key != key));
    }

    // ------------------------------------------------------------------
    // Test 6: cosine of identical direction is ~1.0
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn cosine_identical_direction_is_one() {
        let idx = MemoryVectorIndex::new();
        let v = vec![3.0f32, 4.0]; // magnitude 5
        idx.upsert(RecordKey::new("ns", "col", "a"), v.clone())
            .await
            .unwrap();

        let results = idx.query(&v, 1, &KeyPrefix::default()).await.unwrap();
        assert!(
            (results[0].score - 1.0).abs() < 1e-6,
            "score={}",
            results[0].score
        );
    }

    // ------------------------------------------------------------------
    // Test 7: Embedder trait integration
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn embedder_trait_integration() {
        let embedder = BucketEmbedder;
        let idx = MemoryVectorIndex::new();

        let texts = ["hello", "world", "rust"];
        for text in &texts {
            let vec = embedder.embed(text).await.unwrap();
            idx.upsert(RecordKey::new("ns", "col", *text), vec)
                .await
                .unwrap();
        }

        // Query with the same embedding as "hello" — it should be the top hit.
        let query_vec = embedder.embed("hello").await.unwrap();
        let results = idx
            .query(&query_vec, 3, &KeyPrefix::default())
            .await
            .unwrap();

        assert!(!results.is_empty());
        assert_eq!(results[0].key.id, "hello");
        // Self-similarity must be ~1.0.
        assert!(
            (results[0].score - 1.0).abs() < 1e-6,
            "score={}",
            results[0].score
        );
    }

    // ------------------------------------------------------------------
    // Bonus: remove absent key does not error
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn remove_absent_key_is_ok() {
        let idx = MemoryVectorIndex::new();
        let key = RecordKey::new("ns", "col", "ghost");
        assert!(idx.remove(&key).await.is_ok());
    }

    // ------------------------------------------------------------------
    // Bonus: query dimension mismatch is error
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn query_dimension_mismatch_is_error() {
        let idx = MemoryVectorIndex::new();
        idx.upsert(RecordKey::new("ns", "col", "a"), vec![1.0, 0.0])
            .await
            .unwrap();

        let err = idx
            .query(&[1.0, 0.0, 0.0], 1, &KeyPrefix::default())
            .await
            .unwrap_err();

        assert!(
            matches!(err, CoreError::Backend(ref msg) if msg.contains("dimension mismatch")),
            "unexpected error: {err}"
        );
    }
}
