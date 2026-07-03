//! The generic storage substrate trait and write-outcome types.

use crate::{ContentHash, Record, RecordKey, Result, Revision};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A detected concurrent-edit conflict: the caller's write expected
/// `expected` to be the current revision, but the store holds `current`.
/// Surfaced, never silently resolved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub key: RecordKey,
    pub expected: Option<Revision>,
    pub current: Record,
}

/// The outcome of a conditional write. `Conflict` is a normal, recoverable
/// result — not an error.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "a PutResult may be a Conflict that must be handled, never silently dropped"]
pub enum PutResult {
    Committed(Revision),
    Conflict(Box<Conflict>),
}

/// A pluggable storage substrate over generic records.
#[async_trait]
pub trait Store: Send + Sync {
    /// Fetch a record by key, or `None` if absent.
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>>;

    /// Conditionally write `record`. `expected` is the revision the caller
    /// believes is current (`None` means "expect no existing record").
    /// If the store's current revision differs, returns `PutResult::Conflict`.
    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult>;

    /// List keys matching `prefix`.
    async fn list(&self, prefix: &crate::KeyPrefix) -> Result<Vec<RecordKey>>;
}

/// A content-addressed blob store for out-of-line record bodies
/// ([`Body::Blob`]). Content is keyed by its [`ContentHash`], so byte-identical
/// bodies — e.g. code-graph slices shared across worktrees (ADR 0012) — are
/// stored once. Writes are **write-if-absent**: storing content that already
/// exists is an idempotent no-op, never a conflict (same hash ⇒ same bytes).
///
/// [`Body::Blob`]: crate::Body::Blob
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Store `content` addressed by its hash, write-if-absent, and return the
    /// hash. Idempotent: storing identical content again is a no-op.
    async fn put_blob(&self, content: &[u8]) -> Result<ContentHash>;

    /// Fetch blob content by hash, or `None` if absent.
    async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>>;
}
