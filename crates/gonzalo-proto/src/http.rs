//! JSON DTOs shared by the HTTP/JSON transport (server handlers and the
//! reqwest client). The gRPC transport carries the same `gonzalo-core` types
//! as JSON bytes, so both transports agree on serialization.

use gonzalo_core::{Conflict, Identity, Record, Revision};
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

/// Body of `DELETE /v1/records/{ns}/{col}/{id}`. The key is addressed by the
/// URL path; the body carries the OCC precondition and, optionally, a claimed
/// deleter identity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeleteBody {
    pub expected: Option<Revision>,
    /// The deleter a replicating or admin client names (gonzalo#203). The daemon
    /// honours it only for an admin or open mode; a non-admin is always stamped
    /// as itself (`Principal::delete_author`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Identity>,
}

/// Response of a DELETE: mirrors `gonzalo_core::DeleteResult` on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DeleteOutcome {
    Deleted,
    Conflict { conflict: Box<Conflict> },
}

/// Response of `GET /v1/raw/records/{ns}/{col}/{id}` (gonzalo#203). Absence is
/// `200 {"record": null}`, never `404`: a `404` from this route means the
/// daemon predates replication reads, and the client must tell the two apart
/// without guessing. `PUT` on the same path takes a [`PutBody`] and answers a
/// [`PutOutcome`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawRecordBody {
    pub record: Option<Record>,
}

/// Body of `POST /v1/purge/{ns}/{col}/{id}` (gonzalo#203). The key is addressed
/// by the URL path. `expected` is required, because purge is always conditional
/// on the current revision. The response is a [`DeleteOutcome`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurgeBody {
    pub expected: Revision,
}
