//! The generic storage substrate trait and write-outcome types.

use crate::{Record, RecordKey, Result, Revision};
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
