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
    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
