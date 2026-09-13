//! The generic storage substrate trait and write-outcome types.

use crate::{ContentHash, Identity, Record, RecordKey, Result, Revision};
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
    /// Fetch a record by key. Returns `None` for an absent key **and** for a
    /// tombstoned one: a delete is invisible on this surface, and only
    /// [`get_raw`](Store::get_raw) shows the tombstone. See ADR 0021.
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>>;

    /// Conditionally write `record`. `expected` is the revision the caller
    /// believes is current (`None` means "expect no existing record"). If the
    /// store's current revision differs, returns `PutResult::Conflict`.
    ///
    /// A tombstoned key counts as absent (ADR 0021, spec §8.5):
    /// - `expected == None` **recreates** the key. The store re-stamps
    ///   `record.revision` to continue the chain past the tombstone, so the
    ///   caller must read the real revision back from
    ///   `PutResult::Committed` rather than reuse the one it built.
    /// - Any `Some(_)` — including the tombstone's own revision, which
    ///   consumers never learn — is `Err(CoreError::NotFound)`.
    ///
    /// A `record` whose `kind` is `RecordKind::Tombstone` is always rejected
    /// with an error, on every `current` state: deletes go through
    /// [`delete_as`](Store::delete_as), and replication writes tombstones
    /// through [`put_raw`](Store::put_raw).
    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult>;

    /// List keys matching `prefix`, excluding tombstoned keys. See
    /// [`list_raw`](Store::list_raw) for a listing that includes them.
    async fn list(&self, prefix: &crate::KeyPrefix) -> Result<Vec<RecordKey>>;

    /// Conditionally delete the record at `key`. `expected` is the revision the
    /// caller believes is current: `None` deletes unconditionally; `Some(rev)`
    /// deletes only if the current revision matches, else
    /// `DeleteResult::Conflict`. Deleting an absent key is a no-op `Deleted`.
    /// `Some(author)` records who deleted (stores that write tombstones stamp
    /// it on the tombstone's `meta.author`). See ADR 0018 and ADR 0021.
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult>;

    /// [`delete_as`](Store::delete_as) without an author. Provided; stores
    /// implement `delete_as`.
    async fn delete(&self, key: &RecordKey, expected: Option<Revision>) -> Result<DeleteResult> {
        self.delete_as(key, expected, None).await
    }

    /// Like [`get`](Store::get), but also returns tombstones. For replication
    /// (sync, pull, collection) only: consumers must use `get`. A store must
    /// never implement this by calling `get`, which would hide the deletions
    /// replication exists to carry. See ADR 0021.
    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>>;

    /// Like [`list`](Store::list), but also includes tombstoned keys. For
    /// replication only. See ADR 0021.
    async fn list_raw(&self, prefix: &crate::KeyPrefix) -> Result<Vec<RecordKey>>;

    /// Replication write: store `record` exactly as given, never re-stamping.
    /// A create (`expected == None`) that finds anything stored, tombstones
    /// included, is a `Conflict` carrying it. Sync and pull use this; consumers
    /// must use `put`. A store must never implement this by calling `put`,
    /// whose recreation rule would resurrect records mid-sync. See ADR 0021.
    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult>;

    /// Physically remove the record at `key` only if its current revision is
    /// `expected`; otherwise `DeleteResult::Conflict`. Absent is an idempotent
    /// `Deleted`. The only physical removal in the system: used by tombstone
    /// collection, which must pass the tombstone's revision so a record
    /// recreated in the meantime survives. See ADR 0021.
    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult>;
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
