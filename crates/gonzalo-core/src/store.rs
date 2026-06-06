//! The generic storage substrate trait and write-outcome types.

use crate::{Record, RecordKey, Revision, Result};
use async_trait::async_trait;

/// A detected concurrent-edit conflict: the caller's write expected
/// `expected` but the store holds `current`. `base` is the common ancestor
/// revision if known. Surfaced, never silently resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub key: RecordKey,
    pub expected: Option<Revision>,
    pub current: Record,
}

/// The outcome of a conditional write. `Conflict` is a normal, recoverable
/// result — not an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PutResult {
    Committed(Revision),
    Conflict(Conflict),
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
