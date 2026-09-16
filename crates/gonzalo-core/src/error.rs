//! Core error type. Note: write *conflicts* are NOT errors — they are a
//! typed `PutResult` variant (see `store.rs`). Errors here are genuine
//! failures (I/O, serialization, missing parent for an update).

use crate::RecordKey;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("record not found: {0}")]
    NotFound(RecordKey),
    #[error("serialization error: {0}")]
    Serde(String),
    /// The call itself is wrong, and repeating it unchanged cannot succeed: a
    /// consumer `put` of a tombstone record, for instance, which must go
    /// through `delete_as`. Kept apart from [`Backend`](Self::Backend) so a
    /// caller — and the daemon, which answers `400`/`InvalidArgument` rather
    /// than `500` — can tell "you asked for something impossible" from "the
    /// store failed" (gonzalo#299).
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
