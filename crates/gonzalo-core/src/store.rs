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

/// The outcome of a conditional delete. Like a `Conflict` from `put`, a
/// `Conflict` here is a normal, recoverable result — not an error.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "a DeleteResult may be a Conflict that must be handled, never silently dropped"]
pub enum DeleteResult {
    /// The key is now absent: the record was removed, or there was nothing to
    /// remove (`expected == None`, or an `expected` revision that was already
    /// gone). Idempotent.
    Deleted,
    /// `expected` was supplied but the store's current revision differs; the
    /// record was left untouched and `current` holds the live record.
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

    /// Conditionally delete the record at `key`. `expected` is the revision the
    /// caller believes is current: `None` deletes unconditionally (idempotent
    /// no-op if already absent); `Some(rev)` deletes only if the current revision
    /// matches, returning `DeleteResult::Conflict` if a concurrent write moved it
    /// first. Deleting an already-absent key is a no-op `Deleted`.
    ///
    /// Delete is LOCAL to this store: it is not a tombstone and is NOT propagated
    /// by `sync` — a later sync against a peer that still holds the record copies
    /// it back. See ADR 0018.
    async fn delete(&self, key: &RecordKey, expected: Option<Revision>) -> Result<DeleteResult>;
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

    /// List the hashes of every stored blob. Order is unspecified. Used by GC
    /// to enumerate candidates for sweeping (ADR 0012).
    async fn list_blobs(&self) -> Result<Vec<ContentHash>>;

    /// Delete the blob addressed by `hash`. Deleting an absent blob is an
    /// idempotent no-op — GC may race another sweeper or a re-put.
    async fn delete_blob(&self, hash: &ContentHash) -> Result<()>;
}
