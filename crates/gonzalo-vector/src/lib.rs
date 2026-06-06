//! Vector-search capability layer for gonzalo.
//!
//! Provides the [`VectorIndex`] trait (and its exact in-memory implementation
//! [`MemoryVectorIndex`]) plus the [`Embedder`] seam so caliban-side model
//! providers and gonzalo-hosted embedders share a single interface.
//!
//! All index operations are async so remote / approximate backends can be
//! added in later milestones without breaking callers.

pub mod index;
pub use index::MemoryVectorIndex;

use async_trait::async_trait;
use gonzalo_core::{KeyPrefix, RecordKey, Result};

// ---------------------------------------------------------------------------
// Embedder
// ---------------------------------------------------------------------------

/// Turns text into an embedding vector.
///
/// The default deployment delegates this to the caller (caliban, which talks
/// to model providers); gonzalo can also host its own embedder. This trait
/// is the seam for both.
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

// ---------------------------------------------------------------------------
// Match
// ---------------------------------------------------------------------------

/// One search hit: the record key and its similarity score (cosine, in
/// `[-1.0, 1.0]`; higher is more similar).
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub key: RecordKey,
    pub score: f32,
}

// ---------------------------------------------------------------------------
// VectorIndex
// ---------------------------------------------------------------------------

/// A vector index keyed by [`RecordKey`].
///
/// Async so remote / approximate backends can implement it later; the
/// in-memory impl ([`MemoryVectorIndex`]) is exact brute-force cosine kNN.
#[async_trait]
pub trait VectorIndex: Send + Sync {
    /// Insert or replace the vector for `key`.
    async fn upsert(&self, key: RecordKey, vector: Vec<f32>) -> Result<()>;

    /// Remove `key` if present (no error if absent).
    async fn remove(&self, key: &RecordKey) -> Result<()>;

    /// Return the top-`k` matches to `query`, restricted to keys matching
    /// `filter`, ordered by descending score.
    async fn query(&self, query: &[f32], k: usize, filter: &KeyPrefix) -> Result<Vec<Match>>;
}
