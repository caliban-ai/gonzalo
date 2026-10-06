//! The generic storage substrate trait and write-outcome types.

use crate::{ContentHash, Identity, Record, RecordKey, Result, Revision};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

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
    /// - `expected == None` **recreates** the key, with one exception: a
    ///   *manifest* tombstone (`deleted_kind` of `GraphManifest` or
    ///   `VectorManifest`) holds a restore window, so a create over it is
    ///   refused with `Err(CoreError::Invalid)` (ADR 0030). Restore it with
    ///   `gonzalo undelete`, or discard it with `gonzalo purge`; once `collect`
    ///   removes the tombstone the key is free again. Otherwise the store
    ///   re-stamps `record.revision` to continue the chain past the tombstone,
    ///   so the caller must read the real revision back from
    ///   `PutResult::Committed` rather than reuse the one it built.
    /// - Any `Some(_)` — including the tombstone's own revision, which
    ///   consumers never learn — is `Err(CoreError::NotFound)`.
    ///
    /// A create over a manifest tombstone is therefore a second error case
    /// (`Err(CoreError::Invalid)`, which the daemon answers 400 /
    /// `InvalidArgument`), alongside the following one.
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

/// One stored blob and when it was last written.
///
/// The timestamp is Unix milliseconds rather than a [`SystemTime`] because this
/// type crosses the daemon's HTTP and gRPC surfaces, where milliseconds are the
/// wire form; keeping one type avoids a near-identical DTO in the client and the
/// server. Use [`from_system_time`](Self::from_system_time) and
/// [`age`](Self::age) rather than reading the field arithmetically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobEntry {
    pub hash: ContentHash,
    pub modified_unix_ms: i64,
}

impl BlobEntry {
    pub fn from_system_time(hash: ContentHash, modified: SystemTime) -> Self {
        let modified_unix_ms = match modified.duration_since(std::time::UNIX_EPOCH) {
            // Saturate toward the future: a wrapped value would read as very old,
            // the one direction that deletes data.
            Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
            // Before the epoch: a bogus or wildly-wrong mtime. Represent it
            // faithfully as a negative rather than clamping, so an ordinary
            // negative reads as very old (collectable). In the saturating
            // sub-case (`-i64::MAX`) `age()`'s `checked_sub` overflows and
            // returns `None`, so that blob is KEPT.
            Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
        };
        Self {
            hash,
            modified_unix_ms,
        }
    }

    /// How old this blob is at `now`, or `None` when it is dated in the future.
    ///
    /// Callers treat `None` as "too young to sweep": a blob dated ahead of the
    /// GC host's clock must not be deleted on the strength of a clock
    /// disagreement.
    pub fn age(&self, now: SystemTime) -> Option<Duration> {
        let now_ms = match now.duration_since(std::time::UNIX_EPOCH) {
            // Unlike `from_system_time`, saturating here reads as very OLD: a
            // `now` of `i64::MAX` makes every blob look ancient and collectable.
            // That is a documented exception (ADR 0028), unreachable with a sane
            // clock, and deliberate; do not change the arithmetic.
            Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
            Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
        };
        let age_ms = now_ms.checked_sub(self.modified_unix_ms)?;
        if age_ms < 0 {
            return None;
        }
        Some(Duration::from_millis(age_ms as u64))
    }
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

    /// Every stored blob with the time it was last written. Order is
    /// unspecified, and a hash may repeat — a caller that cares resolves a
    /// duplicate to its newest timestamp. Used by GC to enumerate candidates
    /// and decide which are old enough to sweep (ADR 0024, 0028).
    async fn list_blobs(&self) -> Result<Vec<BlobEntry>>;

    /// Whether `hash` is stored.
    ///
    /// Defaulted so this is not a breaking addition. The default fetches the
    /// blob and throws the bytes away; substrates override it with a cheap
    /// existence check. A writer uses this after committing to confirm the
    /// blobs it referenced are still present (ADR 0028).
    async fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        Ok(self.get_blob(hash).await?.is_some())
    }

    /// Delete the blob addressed by `hash`. Deleting an absent blob is an
    /// idempotent no-op — GC may race another sweeper or a re-put.
    async fn delete_blob(&self, hash: &ContentHash) -> Result<()>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_saturated_pre_epoch_timestamp_has_no_age_and_so_is_kept() {
        // `from_system_time` saturates a pre-epoch mtime to `-i64::MAX`, which a
        // `SystemTime` cannot portably express (it would need to be ~292 million
        // years before the epoch), so the struct is built directly. `age()`'s
        // `checked_sub` overflows and returns `None`, which the sweep treats as
        // too young: the blob is kept, not collected.
        let e = super::BlobEntry {
            hash: super::ContentHash("h".into()),
            modified_unix_ms: -i64::MAX,
        };
        assert_eq!(e.age(std::time::SystemTime::now()), None);
    }

    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn at(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    #[test]
    fn age_is_the_gap_between_modified_and_now() {
        let e = BlobEntry::from_system_time(ContentHash("h".into()), at(1_000));
        assert_eq!(e.age(at(4_000)), Some(Duration::from_millis(3_000)));
    }

    #[test]
    fn age_of_a_blob_dated_in_the_future_is_none() {
        // The GC host's clock behind the store's. `None` means "too young", so
        // the sweep keeps the blob — ambiguity errs toward keeping data.
        let e = BlobEntry::from_system_time(ContentHash("h".into()), at(9_000));
        assert_eq!(e.age(at(1_000)), None);
    }

    #[test]
    fn age_at_exactly_now_is_zero_not_none() {
        let e = BlobEntry::from_system_time(ContentHash("h".into()), at(5_000));
        assert_eq!(e.age(at(5_000)), Some(Duration::ZERO));
    }

    #[test]
    fn a_pre_epoch_timestamp_round_trips_as_a_negative_and_still_ages() {
        let before_epoch = UNIX_EPOCH - Duration::from_millis(500);
        let e = BlobEntry::from_system_time(ContentHash("h".into()), before_epoch);
        assert!(e.modified_unix_ms < 0);
        assert_eq!(e.age(at(500)), Some(Duration::from_millis(1_000)));
    }
}
