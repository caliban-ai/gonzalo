//! JSON DTOs shared by the HTTP/JSON transport (server handlers and the
//! reqwest client). The gRPC transport carries the same `gonzalo-core` types
//! as JSON bytes, so both transports agree on serialization.

use gonzalo_core::{Conflict, Record, Revision};
use serde::{Deserialize, Serialize};

/// Body of `PUT /v1/records/{ns}/{col}/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutBody {
    pub record: Record,
    pub expected: Option<Revision>,
}

/// Response of a PUT: mirrors `gonzalo_core::PutResult` on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PutOutcome {
    Committed { revision: Revision },
    Conflict { conflict: Box<Conflict> },
}
