//! gRPC transport: adapts the generated `Gonzalo` service to the shared
//! `Service`, carrying `gonzalo-core` types as JSON payloads.

use crate::Service;
use crate::auth::{Access, Auth, Principal};
use gonzalo_core::{
    ContentHash, CoreError, DeleteResult, Identity, KeyPrefix, PutResult, Record, RecordKey,
    Revision,
};
use gonzalo_proto::v1::{
    DeleteBlobRequest, DeleteBlobResponse, DeleteRequest, DeleteResponse, GetBlobRequest,
    GetBlobResponse, GetRequest, GetResponse, GraphLocatedResponse, GraphNamesResponse,
    GraphQueryRequest, ListBlobsRequest, ListBlobsResponse, ListRequest, ListResponse,
    PurgeRequest, PurgeResponse, PutBlobRequest, PutBlobResponse, PutRequest, PutResponse,
    TicketSyncRequest, TicketSyncResponse,
    gonzalo_server::{Gonzalo, GonzaloServer},
};
use serde::Serialize;
use std::sync::Arc;
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};

/// Reserved authz namespace for namespace-agnostic blob ops (ADR 0015), matching
/// the HTTP transport.
const BLOB_NS: &str = "_blobs";

/// Adapts [`Service`] to the generated gRPC trait, enforcing namespace-scoped
/// auth (ADR 0015) per call from the request's bearer metadata.
pub struct GrpcAdapter {
    service: Service,
    auth: Arc<Auth>,
}

impl GrpcAdapter {
    /// Adapter with auth disabled (open) — used by tests and open deployments.
    pub fn new(service: Service) -> Self {
        Self::with_auth(service, Arc::new(Auth::Disabled))
    }

    /// Adapter enforcing `auth`.
    pub fn with_auth(service: Service, auth: Arc<Auth>) -> Self {
        Self { service, auth }
    }

    /// Authenticate the call's bearer token and authorize `access` on
    /// `namespace`. Returns the [`Principal`] (for author stamping on writes).
    #[allow(clippy::result_large_err)]
    fn authorize(
        &self,
        metadata: &MetadataMap,
        access: Access,
        namespace: &str,
    ) -> Result<Principal, Status> {
        let principal = self.authenticate(metadata)?;
        self.check_access(&principal, access, namespace)?;
        Ok(principal)
    }

    /// Authenticate the call's bearer token into a [`Principal`], independent of
    /// any namespace. Split out from [`authorize`] so a handler can reject an
    /// unauthenticated caller *before* deserializing attacker-controlled JSON
    /// (#146) and only then authorize against a namespace parsed from the body.
    #[allow(clippy::result_large_err)]
    fn authenticate(&self, metadata: &MetadataMap) -> Result<Principal, Status> {
        self.auth
            .authenticate(bearer(metadata))
            .ok_or_else(|| Status::unauthenticated("invalid or missing token"))
    }

    /// Authorize an already-authenticated `principal` for `access` on `namespace`.
    #[allow(clippy::result_large_err)]
    fn check_access(
        &self,
        principal: &Principal,
        access: Access,
        namespace: &str,
    ) -> Result<(), Status> {
        if principal.allows(access, namespace) {
            Ok(())
        } else {
            Err(Status::permission_denied(format!(
                "principal {:?} lacks {access:?} on namespace {namespace:?}",
                principal.name()
            )))
        }
    }

    /// Authenticate the call and require an admin principal (`read` and
    /// `write` on `"*"`). Used by `purge` (gonzalo#203): an early purge is data
    /// loss that surfaces later on another machine, so no namespace scope is
    /// enough. Takes no body, so it runs before any deserialization (#146).
    #[allow(clippy::result_large_err)]
    fn authorize_admin(
        &self,
        metadata: &MetadataMap,
        operation: &str,
    ) -> Result<Principal, Status> {
        let principal = self.authenticate(metadata)?;
        if principal.is_admin() {
            Ok(principal)
        } else {
            Err(Status::permission_denied(format!(
                "principal {:?} is not an admin; {operation} requires admin",
                principal.name()
            )))
        }
    }

    /// Authenticate, parse a `PutRequest` (malformed → `InvalidArgument`) and
    /// authorize `Write` on the record's namespace. Shared by `Put` and `PutRaw`,
    /// in #146 order.
    #[allow(clippy::result_large_err)]
    fn authorize_put(
        &self,
        metadata: &MetadataMap,
        r: &PutRequest,
    ) -> Result<(Principal, Record, Option<Revision>), Status> {
        let principal = self.authenticate(metadata)?;
        let record: Record = serde_json::from_slice(&r.record_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let expected: Option<Revision> = serde_json::from_slice(&r.expected_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.check_access(&principal, Access::Write, &record.key.namespace)?;
        Ok((principal, record, expected))
    }

    /// Authenticate the call and authorize a key listing (`read` on the
    /// namespace, or on `"*"` when unscoped), then build its prefix. Shared by
    /// `List` and `ListRaw`.
    #[allow(clippy::result_large_err)]
    fn authorize_list(&self, metadata: &MetadataMap, r: ListRequest) -> Result<KeyPrefix, Status> {
        self.authorize(
            metadata,
            Access::Read,
            r.namespace.as_deref().unwrap_or("*"),
        )?;
        Ok(KeyPrefix {
            namespace: r.namespace,
            collection: r.collection,
        })
    }
}

/// The bearer token from gRPC `authorization: Bearer <token>` metadata.
fn bearer(metadata: &MetadataMap) -> Option<&str> {
    metadata
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Map a backend failure to an opaque `Internal` status. The full error is
/// logged server-side; the client sees only "internal error" so on-disk graph
/// paths, SQLite text, and S3 endpoint/bucket detail never leak to the network
/// (#148).
fn internal<E: std::fmt::Display>(e: E) -> Status {
    eprintln!("gonzalod: internal error: {e}");
    Status::internal("internal error")
}

#[tonic::async_trait]
impl Gonzalo for GrpcAdapter {
    async fn get(&self, req: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Read, &r.namespace)?;
        let key = RecordKey::new(r.namespace, r.collection, r.id);
        let rec = self.service.get(&key).await.map_err(internal)?;
        Ok(Response::new(get_response(rec)?))
    }

    async fn put(&self, req: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Authenticate BEFORE deserializing attacker-controlled JSON (#146): an
        // unauthenticated caller is rejected without ever feeding its body to
        // serde. Only then parse the body (malformed input is the caller's
        // error → invalid_argument, not internal) and authorize the write
        // against the namespace named in the record's key.
        let (principal, mut record, expected) = self.authorize_put(&metadata, &r)?;
        // Stamp the author from the authenticated principal — unforgeable (ADR
        // 0015). Open mode (no auth) leaves the record's author untouched.
        if principal.is_authenticated() {
            record.meta.author = Identity::new(principal.name());
        }
        let outcome = self
            .service
            .put(record, expected)
            .await
            .map_err(put_error)?;
        Ok(Response::new(put_response(outcome)?))
    }

    async fn delete(
        &self,
        req: Request<DeleteRequest>,
    ) -> Result<Response<DeleteResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Authenticate BEFORE deserializing attacker-controlled JSON (#146), then
        // parse the precondition and the optional claimed-author (malformed
        // input is the caller's error → invalid_argument), authorize the write
        // against the path's namespace, and build the key from
        // (namespace, collection, id). The tombstone's author follows
        // `Principal::delete_author` (gonzalo#203, mirrors `put_raw`'s
        // authorship rule): a non-admin is always stamped as itself and any
        // claimed author is ignored; an admin or open mode may name the
        // deleter via `author_json`; and otherwise an authenticated admin is
        // stamped as itself while open mode keeps the prior author (no
        // identity to stamp, ADR 0015).
        let principal = self.authenticate(&metadata)?;
        let expected: Option<Revision> = serde_json::from_slice(&r.expected_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let claimed: Option<Identity> = if r.author_json.is_empty() {
            None
        } else {
            Some(
                serde_json::from_slice(&r.author_json)
                    .map_err(|e| Status::invalid_argument(e.to_string()))?,
            )
        };
        self.check_access(&principal, Access::Write, &r.namespace)?;
        let author = principal.delete_author(claimed);
        let key = RecordKey::new(r.namespace, r.collection, r.id);
        let outcome = self
            .service
            .delete_as(&key, expected, author)
            .await
            .map_err(internal)?;
        let (outcome, payload_json) = delete_outcome_parts(outcome)?;
        Ok(Response::new(DeleteResponse {
            outcome,
            payload_json,
        }))
    }

    async fn list(&self, req: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        let prefix = self.authorize_list(&metadata, r)?;
        let keys = self.service.list(&prefix).await.map_err(internal)?;
        Ok(Response::new(list_response(&keys)?))
    }

    async fn get_raw(&self, req: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Read, &r.namespace)?;
        let key = RecordKey::new(r.namespace, r.collection, r.id);
        let rec = self.service.get_raw(&key).await.map_err(internal)?;
        Ok(Response::new(get_response(rec)?))
    }

    async fn list_raw(&self, req: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Same authorization as `List`: unscoped requires `read` on `"*"`.
        let prefix = self.authorize_list(&metadata, r)?;
        let keys = self.service.list_raw(&prefix).await.map_err(internal)?;
        Ok(Response::new(list_response(&keys)?))
    }

    async fn put_raw(&self, req: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Replication write (gonzalo#203). Authorship stays unforgeable (ADR
        // 0015): an admin token is the replication credential, so an admin's
        // PutRaw keeps the replicated record's original author. Any other
        // principal is restamped exactly as `Put` restamps. Open mode's
        // implicit principal is an admin, so it keeps the author too.
        let (principal, mut record, expected) = self.authorize_put(&metadata, &r)?;
        if !principal.is_admin() {
            record.meta.author = Identity::new(principal.name());
        }
        let outcome = self
            .service
            .put_raw(record, expected)
            .await
            .map_err(put_error)?;
        Ok(Response::new(put_response(outcome)?))
    }

    async fn purge(&self, req: Request<PurgeRequest>) -> Result<Response<PurgeResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Admin check first, then parse the attacker-controlled precondition
        // (malformed → invalid_argument, the caller's error).
        self.authorize_admin(&metadata, "purge")?;
        let expected: Revision = serde_json::from_slice(&r.expected_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let key = RecordKey::new(r.namespace, r.collection, r.id);
        let outcome = self.service.purge(&key, expected).await.map_err(internal)?;
        let (outcome, payload_json) = delete_outcome_parts(outcome)?;
        Ok(Response::new(PurgeResponse {
            outcome,
            payload_json,
        }))
    }

    async fn ticket_sync(
        &self,
        req: Request<TicketSyncRequest>,
    ) -> Result<Response<TicketSyncResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Ticket sync writes records in the `tickets` namespace.
        self.authorize(&metadata, Access::Write, "tickets")?;
        let conn: gonzalo_ticket_config::Connection = serde_json::from_slice(&r.connection_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let summary = self
            .service
            .ticket_sync(&conn, "gonzalod")
            .await
            .map_err(|e| match e {
                // A misconfigured request is the caller's own input → safe to
                // echo. An internal failure goes through `internal` so its
                // detail is logged, not leaked (#148).
                crate::service::TicketSyncError::BadRequest(m) => Status::invalid_argument(m),
                crate::service::TicketSyncError::Internal(m) => internal(m),
            })?;
        Ok(Response::new(TicketSyncResponse {
            imported: summary.imported as u64,
            updated: summary.updated as u64,
            unchanged: summary.unchanged as u64,
        }))
    }

    async fn graph_definitions(
        &self,
        req: Request<GraphQueryRequest>,
    ) -> Result<Response<GraphLocatedResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Read, &r.repo)?;
        let items = self
            .service
            .graph_definitions(&r.repo, &r.view_id, &r.name)
            .await
            .map_err(internal)?;
        Ok(Response::new(located_response(&items)?))
    }

    async fn graph_references_to(
        &self,
        req: Request<GraphQueryRequest>,
    ) -> Result<Response<GraphLocatedResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Read, &r.repo)?;
        let items = self
            .service
            .graph_references_to(&r.repo, &r.view_id, &r.name)
            .await
            .map_err(internal)?;
        Ok(Response::new(located_response(&items)?))
    }

    async fn graph_callers_of(
        &self,
        req: Request<GraphQueryRequest>,
    ) -> Result<Response<GraphNamesResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Read, &r.repo)?;
        let names = self
            .service
            .graph_callers_of(&r.repo, &r.view_id, &r.name)
            .await
            .map_err(internal)?;
        Ok(Response::new(GraphNamesResponse { names }))
    }

    async fn graph_callees(
        &self,
        req: Request<GraphQueryRequest>,
    ) -> Result<Response<GraphNamesResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Read, &r.repo)?;
        let names = self
            .service
            .graph_callees(&r.repo, &r.view_id, &r.name)
            .await
            .map_err(internal)?;
        Ok(Response::new(GraphNamesResponse { names }))
    }

    async fn graph_impact(
        &self,
        req: Request<GraphQueryRequest>,
    ) -> Result<Response<GraphNamesResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Read, &r.repo)?;
        let names = self
            .service
            .graph_impact_names(&r.repo, &r.view_id, &r.name)
            .await
            .map_err(internal)?;
        Ok(Response::new(GraphNamesResponse { names }))
    }

    async fn put_blob(
        &self,
        req: Request<PutBlobRequest>,
    ) -> Result<Response<PutBlobResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Write, BLOB_NS)?;
        // Verify the content hashes to the advertised value before writing —
        // same integrity check as the HTTP hash-addressed PUT.
        let computed = ContentHash::of(&r.content);
        if computed.0 != r.hash {
            return Err(Status::invalid_argument(
                "blob content does not match the advertised hash",
            ));
        }
        let hash = self.service.put_blob(&r.content).await.map_err(internal)?;
        Ok(Response::new(PutBlobResponse { hash: hash.0 }))
    }

    async fn get_blob(
        &self,
        req: Request<GetBlobRequest>,
    ) -> Result<Response<GetBlobResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Read, BLOB_NS)?;
        let found = self
            .service
            .get_blob(&ContentHash(r.hash))
            .await
            .map_err(internal)?;
        let resp = match found {
            Some(content) => GetBlobResponse {
                found: true,
                content,
            },
            None => GetBlobResponse {
                found: false,
                content: Vec::new(),
            },
        };
        Ok(Response::new(resp))
    }

    async fn list_blobs(
        &self,
        req: Request<ListBlobsRequest>,
    ) -> Result<Response<ListBlobsResponse>, Status> {
        let (metadata, _ext, _r) = req.into_parts();
        self.authorize(&metadata, Access::Read, BLOB_NS)?;
        let hashes = self.service.list_blobs().await.map_err(internal)?;
        Ok(Response::new(ListBlobsResponse {
            hashes: hashes.into_iter().map(|h| h.0).collect(),
        }))
    }

    async fn delete_blob(
        &self,
        req: Request<DeleteBlobRequest>,
    ) -> Result<Response<DeleteBlobResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Write, BLOB_NS)?;
        self.service
            .delete_blob(&ContentHash(r.hash))
            .await
            .map_err(internal)?;
        Ok(Response::new(DeleteBlobResponse {}))
    }
}

/// Map a put failure. `CoreError::NotFound` (`expected` names a revision the
/// store does not hold) is the caller's precondition failing →
/// `FailedPrecondition`, which the client maps back to `NotFound`. Everything
/// else is opaque (#148).
fn put_error(e: CoreError) -> Status {
    match e {
        CoreError::NotFound(key) => Status::failed_precondition(format!("record not found: {key}")),
        other => internal(other),
    }
}

/// `PutResponse` for a `PutResult` (shared by `Put` and `PutRaw`).
#[allow(clippy::result_large_err)]
fn put_response(outcome: PutResult) -> Result<PutResponse, Status> {
    Ok(match outcome {
        PutResult::Committed(rev) => PutResponse {
            outcome: "committed".into(),
            payload_json: serde_json::to_vec(&rev).map_err(internal)?,
        },
        PutResult::Conflict(c) => PutResponse {
            outcome: "conflict".into(),
            payload_json: serde_json::to_vec(&*c).map_err(internal)?,
        },
    })
}

/// `GetResponse` for an optional record (shared by `Get` and `GetRaw`).
#[allow(clippy::result_large_err)]
fn get_response(rec: Option<Record>) -> Result<GetResponse, Status> {
    Ok(match rec {
        Some(rec) => GetResponse {
            found: true,
            record_json: serde_json::to_vec(&rec).map_err(internal)?,
        },
        None => GetResponse {
            found: false,
            record_json: Vec::new(),
        },
    })
}

/// `ListResponse` of JSON-encoded keys (shared by `List` and `ListRaw`).
#[allow(clippy::result_large_err)]
fn list_response(keys: &[RecordKey]) -> Result<ListResponse, Status> {
    let keys_json = keys
        .iter()
        .map(serde_json::to_vec)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(internal)?;
    Ok(ListResponse { keys_json })
}

/// `(outcome, payload_json)` for a `DeleteResult` (shared by `Delete` and
/// `Purge`): `"deleted"` + empty, or `"conflict"` + JSON of `Conflict`.
#[allow(clippy::result_large_err)]
fn delete_outcome_parts(outcome: DeleteResult) -> Result<(String, Vec<u8>), Status> {
    Ok(match outcome {
        DeleteResult::Deleted => ("deleted".into(), Vec::new()),
        DeleteResult::Conflict(c) => (
            "conflict".into(),
            serde_json::to_vec(&*c).map_err(internal)?,
        ),
    })
}

/// JSON-encode each located item into a `GraphLocatedResponse` (the shared
/// JSON-in-bytes convention).
// `Status` is large but fixed by tonic's API, so the large-err lint can't be
// acted on (same as `serve_grpc`).
#[allow(clippy::result_large_err)]
fn located_response<T: Serialize>(items: &[T]) -> Result<GraphLocatedResponse, Status> {
    let items_json = items
        .iter()
        .map(serde_json::to_vec)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(internal)?;
    Ok(GraphLocatedResponse { items_json })
}

/// Serve gRPC on an already-bound listener until the process ends. `auth`
/// governs per-call namespace authorization (ADR 0015); `Auth::Disabled` serves
/// open.
///
/// # The decode ceiling applies to every RPC, not just blobs
///
/// `max_decoding_message_size` below is raised to `max_blob_size` so 64 MiB
/// blobs can transit `PutBlob`. tonic scopes that setting **per server**, not
/// per method — there is no per-RPC knob — so it raises the decode ceiling for
/// *every* RPC, including record `Put`, from tonic's 4 MiB default to
/// `GONZALO_MAX_BLOB_SIZE`.
///
/// Two consequences worth knowing before you change the env var:
///
/// - Raising `GONZALO_MAX_BLOB_SIZE` to allow larger blobs also allows larger
///   records. That is a surprising reach for a knob named for blobs.
/// - It is asymmetric with HTTP, where the blob body limit is scoped to the
///   blob sub-router, so record `PUT`s keep axum's 2 MiB default (`http.rs`).
///
/// This is bounded and every call is authenticated, so it is documented rather
/// than closed (#194). Closing it would mean a tower layer inspecting
/// content-length by gRPC method path — real machinery for a benign gap.
pub async fn serve_grpc(
    listener: tokio::net::TcpListener,
    service: Service,
    auth: Arc<Auth>,
) -> Result<(), tonic::transport::Error> {
    let max_blob = service.max_blob_size();
    let adapter = GrpcAdapter::with_auth(service, auth);
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tonic::transport::Server::builder()
        // Per-server, so this governs record RPCs too — see the doc comment.
        .add_service(GonzaloServer::new(adapter).max_decoding_message_size(max_blob))
        .serve_with_incoming(incoming)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_core::{BlobStore, Identity, Manifest, Meta, RecordKind, Store};
    use gonzalo_graph::{Located, Symbol, build_rust};
    use gonzalo_store_fs::FsStore;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    /// Seed one view (`r`/`main`) with two slices and return a gRPC adapter over it.
    async fn seeded_adapter() -> GrpcAdapter {
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        let mut manifest = Manifest::new();
        for (path, src) in [
            ("lib.rs", "fn helper() {}"),
            ("main.rs", "fn main() { helper(); }"),
        ] {
            let hash = fs
                .put_blob(&build_rust(src).to_slice_bytes())
                .await
                .unwrap();
            manifest.insert(path, hash);
        }
        let body = manifest.to_body();
        let record = Record {
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            kind: RecordKind::GraphManifest,
            meta: Meta {
                author: Identity::new("tester"),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            key: Manifest::key("r", "main"),
            ancestors: Vec::new(),
            deleted_at: None,
        };
        let outcome = fs.put(record, None).await.unwrap();
        assert!(matches!(outcome, PutResult::Committed(_)));
        GrpcAdapter::new(Service::new(fs.clone(), fs))
    }

    fn query(name: &str) -> Request<GraphQueryRequest> {
        Request::new(GraphQueryRequest {
            repo: "r".into(),
            view_id: "main".into(),
            name: name.into(),
        })
    }

    #[tokio::test]
    async fn graph_definitions_returns_located_json() {
        let adapter = seeded_adapter().await;
        let resp = adapter
            .graph_definitions(query("helper"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.items_json.len(), 1);
        let located: Located<Symbol> = serde_json::from_slice(&resp.items_json[0]).unwrap();
        assert_eq!(located.path, "lib.rs");
        assert_eq!(located.item.name, "helper");
    }

    // --- namespace-scoped auth (ADR 0015) ---

    use std::collections::HashMap;

    /// A `writer` principal scoped to the `memory` namespace, plus an `admin`.
    fn scoped_auth() -> Arc<Auth> {
        Arc::new(Auth::Enabled(HashMap::from([
            (
                "wtok".to_string(),
                Principal::new("writer", vec!["memory".into()], vec!["memory".into()]),
            ),
            ("atok".to_string(), Principal::admin("admin")),
        ])))
    }

    fn with_token<T>(msg: T, token: &str) -> Request<T> {
        let mut req = Request::new(msg);
        req.metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        req
    }

    fn fs_adapter(auth: Arc<Auth>) -> GrpcAdapter {
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        GrpcAdapter::with_auth(Service::new(fs.clone(), fs), auth)
    }

    fn get_req(namespace: &str) -> GetRequest {
        GetRequest {
            namespace: namespace.into(),
            collection: "col".into(),
            id: "x".into(),
        }
    }

    fn put_req(namespace: &str, author: &str) -> PutRequest {
        let record = Record {
            revision: Revision::initial(b"{}"),
            parent: None,
            body: gonzalo_core::Body::Inline(b"{}".to_vec()),
            kind: RecordKind::MemoryTier,
            meta: Meta {
                author: Identity::new(author),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            key: RecordKey::new(namespace, "col", "x"),
            ancestors: Vec::new(),
            deleted_at: None,
        };
        PutRequest {
            record_json: serde_json::to_vec(&record).unwrap(),
            expected_json: serde_json::to_vec(&Option::<Revision>::None).unwrap(),
        }
    }

    /// A well-formed `PutRequest` whose `record_json` is not valid JSON.
    fn malformed_put_req() -> PutRequest {
        PutRequest {
            record_json: b"definitely not a record".to_vec(),
            expected_json: serde_json::to_vec(&Option::<Revision>::None).unwrap(),
        }
    }

    /// A store whose every op fails, to force the `internal` (server-error) path.
    struct DownStore;

    fn leaky() -> gonzalo_core::CoreError {
        gonzalo_core::CoreError::Backend("s3://secret-bucket".into())
    }

    #[async_trait::async_trait]
    impl Store for DownStore {
        async fn get(&self, _key: &RecordKey) -> gonzalo_core::Result<Option<Record>> {
            Err(gonzalo_core::CoreError::Backend(
                "/var/lib/gonzalo/graphs/secret.sqlite unreachable".into(),
            ))
        }
        async fn put(
            &self,
            _record: Record,
            _expected: Option<Revision>,
        ) -> gonzalo_core::Result<PutResult> {
            Err(leaky())
        }
        async fn list(
            &self,
            _prefix: &gonzalo_core::KeyPrefix,
        ) -> gonzalo_core::Result<Vec<RecordKey>> {
            Err(leaky())
        }
        async fn delete_as(
            &self,
            _key: &RecordKey,
            _expected: Option<Revision>,
            _author: Option<Identity>,
        ) -> gonzalo_core::Result<DeleteResult> {
            Err(leaky())
        }
        async fn get_raw(&self, _key: &RecordKey) -> gonzalo_core::Result<Option<Record>> {
            Err(leaky())
        }
        async fn list_raw(
            &self,
            _prefix: &gonzalo_core::KeyPrefix,
        ) -> gonzalo_core::Result<Vec<RecordKey>> {
            Err(leaky())
        }
        async fn put_raw(
            &self,
            _record: Record,
            _expected: Option<Revision>,
        ) -> gonzalo_core::Result<PutResult> {
            Err(leaky())
        }
        async fn purge(
            &self,
            _key: &RecordKey,
            _expected: Revision,
        ) -> gonzalo_core::Result<DeleteResult> {
            Err(leaky())
        }
    }

    #[tokio::test]
    async fn put_authenticates_before_deserializing() {
        // A malformed body with NO token is rejected at authentication, before
        // serde ever runs on the attacker-controlled JSON (#146).
        let adapter = fs_adapter(scoped_auth());
        let err = adapter
            .put(Request::new(malformed_put_req()))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn put_malformed_body_is_invalid_argument_not_internal() {
        // An authorized caller sending a malformed body gets InvalidArgument —
        // the caller's own bad input — not Internal (#146).
        let adapter = fs_adapter(scoped_auth());
        let err = adapter
            .put(with_token(malformed_put_req(), "wtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn backend_error_is_opaque() {
        // A forced backend failure yields an opaque Internal status; the leaky
        // path/bucket detail never reaches the client (#148).
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        let adapter = GrpcAdapter::new(Service::new(Arc::new(DownStore), fs));
        let err = adapter.get(Request::new(get_req("any"))).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Internal);
        assert_eq!(err.message(), "internal error");
        assert!(!err.message().contains("secret"));
    }

    #[tokio::test]
    async fn missing_token_is_unauthenticated() {
        let adapter = fs_adapter(scoped_auth());
        let err = adapter
            .get(Request::new(get_req("memory")))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn read_is_allowed_in_scope_denied_out_of_scope() {
        let adapter = fs_adapter(scoped_auth());
        // In-scope read succeeds (record absent → found:false, but authorized).
        assert!(
            adapter
                .get(with_token(get_req("memory"), "wtok"))
                .await
                .is_ok()
        );
        // Out-of-scope read is denied.
        let err = adapter
            .get(with_token(get_req("secrets"), "wtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn write_is_scoped_and_author_is_stamped() {
        let adapter = fs_adapter(scoped_auth());
        // Write outside scope is denied.
        let err = adapter
            .put(with_token(put_req("secrets", "writer"), "wtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        // In-scope write commits — even though the client claimed author
        // "forged", the daemon stamps the authenticated principal.
        adapter
            .put(with_token(put_req("memory", "forged"), "wtok"))
            .await
            .unwrap();
        let resp = adapter
            .get(with_token(get_req("memory"), "wtok"))
            .await
            .unwrap()
            .into_inner();
        let record: Record = serde_json::from_slice(&resp.record_json).unwrap();
        assert_eq!(record.meta.author, Identity::new("writer"));
    }

    #[tokio::test]
    async fn list_without_namespace_requires_admin() {
        let adapter = fs_adapter(scoped_auth());
        let scoped = adapter
            .list(with_token(ListRequest::default(), "wtok"))
            .await
            .unwrap_err();
        assert_eq!(scoped.code(), tonic::Code::PermissionDenied);
        // Admin (wildcard) may list across all namespaces.
        assert!(
            adapter
                .list(with_token(ListRequest::default(), "atok"))
                .await
                .is_ok()
        );
    }

    // --- blob RPCs (#184) ---

    #[tokio::test]
    async fn grpc_blob_roundtrip_open() {
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        let adapter = GrpcAdapter::new(Service::new(fs.clone(), fs));
        let content = b"grpc blob body".to_vec();
        let hash = gonzalo_core::ContentHash::of(&content).0;

        let put = adapter
            .put_blob(Request::new(PutBlobRequest {
                hash: hash.clone(),
                content: content.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(put.hash, hash);

        let got = adapter
            .get_blob(Request::new(GetBlobRequest { hash: hash.clone() }))
            .await
            .unwrap()
            .into_inner();
        assert!(got.found);
        assert_eq!(got.content, content);

        let listed = adapter
            .list_blobs(Request::new(ListBlobsRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(listed.hashes, vec![hash.clone()]);

        adapter
            .delete_blob(Request::new(DeleteBlobRequest { hash: hash.clone() }))
            .await
            .unwrap();
        let gone = adapter
            .get_blob(Request::new(GetBlobRequest { hash }))
            .await
            .unwrap()
            .into_inner();
        assert!(!gone.found);
    }

    #[tokio::test]
    async fn grpc_put_blob_hash_mismatch_is_invalid_argument() {
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        let adapter = GrpcAdapter::new(Service::new(fs.clone(), fs));
        let err = adapter
            .put_blob(Request::new(PutBlobRequest {
                hash: gonzalo_core::ContentHash::of(b"not the body").0,
                content: b"the body".to_vec(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn grpc_blob_ops_require_blobs_scope() {
        // `scoped_auth()` grants `memory` only, not `_blobs`.
        let adapter = fs_adapter(scoped_auth());
        let content = b"scoped".to_vec();
        let hash = gonzalo_core::ContentHash::of(&content).0;

        let denied = adapter
            .put_blob(with_token(
                PutBlobRequest {
                    hash: hash.clone(),
                    content: content.clone(),
                },
                "wtok",
            ))
            .await
            .unwrap_err();
        assert_eq!(denied.code(), tonic::Code::PermissionDenied);

        // Admin token succeeds.
        adapter
            .put_blob(with_token(PutBlobRequest { hash, content }, "atok"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn graph_name_queries_return_names() {
        let adapter = seeded_adapter().await;
        let impact = adapter
            .graph_impact(query("helper"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(impact.names, vec!["main".to_string()]);
        let callees = adapter
            .graph_callees(query("main"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(callees.names, vec!["helper".to_string()]);
        let callers = adapter
            .graph_callers_of(query("helper"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(callers.names, vec!["main".to_string()]);
    }

    // --- replication RPCs: GetRaw / ListRaw / PutRaw / Purge (gonzalo#203) ---

    /// `reader` reads `memory` only; `writer` reads and writes `memory`; `admin`.
    fn tomb_auth() -> Arc<Auth> {
        Arc::new(Auth::Enabled(HashMap::from([
            (
                "rtok".to_string(),
                Principal::new("reader", vec!["memory".into()], vec![]),
            ),
            (
                "wtok".to_string(),
                Principal::new("writer", vec!["memory".into()], vec!["memory".into()]),
            ),
            ("atok".to_string(), Principal::admin("admin")),
        ])))
    }

    fn delete_req(namespace: &str) -> DeleteRequest {
        DeleteRequest {
            namespace: namespace.into(),
            collection: "col".into(),
            id: "x".into(),
            expected_json: serde_json::to_vec(&Option::<Revision>::None).unwrap(),
            author_json: Vec::new(),
        }
    }

    fn purge_req(namespace: &str, expected: &Revision) -> PurgeRequest {
        PurgeRequest {
            namespace: namespace.into(),
            collection: "col".into(),
            id: "x".into(),
            expected_json: serde_json::to_vec(expected).unwrap(),
        }
    }

    /// A `PutRequest` for memory/col/x with an explicit author and precondition.
    fn put_req_with(author: &str, payload: &[u8], expected: Option<Revision>) -> PutRequest {
        let body = gonzalo_core::Body::Inline(payload.to_vec());
        let record = Record {
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            kind: RecordKind::MemoryTier,
            meta: Meta {
                author: Identity::new(author),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            ancestors: Vec::new(),
            deleted_at: None,
            key: RecordKey::new("memory", "col", "x"),
        };
        PutRequest {
            record_json: serde_json::to_vec(&record).unwrap(),
            expected_json: serde_json::to_vec(&expected).unwrap(),
        }
    }

    fn memory_list() -> ListRequest {
        ListRequest {
            namespace: Some("memory".into()),
            collection: None,
        }
    }

    async fn raw_record(adapter: &GrpcAdapter) -> Option<Record> {
        let raw = adapter
            .get_raw(with_token(get_req("memory"), "atok"))
            .await
            .unwrap()
            .into_inner();
        raw.found
            .then(|| serde_json::from_slice(&raw.record_json).unwrap())
    }

    /// Put memory/col/x as `writer`, delete it with `delete_token`; return the
    /// tombstone.
    async fn seed_tombstone(adapter: &GrpcAdapter, delete_token: &str) -> Record {
        adapter
            .put(with_token(put_req("memory", "writer"), "wtok"))
            .await
            .unwrap();
        let del = adapter
            .delete(with_token(delete_req("memory"), delete_token))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(del.outcome, "deleted");
        raw_record(adapter).await.expect("tombstone")
    }

    fn decode_keys(resp: ListResponse) -> Vec<RecordKey> {
        resp.keys_json
            .iter()
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn grpc_delete_leaves_a_stamped_tombstone_for_raw_reads() {
        let adapter = fs_adapter(tomb_auth());
        // `writer` wrote it; `admin` deletes it; the tombstone names the deleter.
        let tomb = seed_tombstone(&adapter, "atok").await;
        assert_eq!(tomb.kind, RecordKind::Tombstone);
        assert_eq!(tomb.revision.hash, gonzalo_core::tombstone_hash());
        assert_eq!(tomb.meta.author, Identity::new("admin"));
        let key = RecordKey::new("memory", "col", "x");

        let got = adapter
            .get(with_token(get_req("memory"), "rtok"))
            .await
            .unwrap()
            .into_inner();
        assert!(!got.found);
        let live = decode_keys(
            adapter
                .list(with_token(memory_list(), "rtok"))
                .await
                .unwrap()
                .into_inner(),
        );
        assert!(!live.contains(&key));

        let raw = adapter
            .get_raw(with_token(get_req("memory"), "rtok"))
            .await
            .unwrap()
            .into_inner();
        assert!(raw.found);
        let raw_keys = decode_keys(
            adapter
                .list_raw(with_token(memory_list(), "rtok"))
                .await
                .unwrap()
                .into_inner(),
        );
        assert!(raw_keys.contains(&key));
    }

    #[tokio::test]
    async fn grpc_raw_reads_are_read_scoped() {
        let adapter = fs_adapter(tomb_auth());
        let err = adapter
            .get_raw(with_token(get_req("secrets"), "rtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        let err = adapter
            .get_raw(Request::new(get_req("memory")))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn grpc_unscoped_list_raw_requires_admin() {
        let adapter = fs_adapter(tomb_auth());
        for token in ["rtok", "wtok"] {
            let err = adapter
                .list_raw(with_token(ListRequest::default(), token))
                .await
                .unwrap_err();
            assert_eq!(err.code(), tonic::Code::PermissionDenied, "{token}");
        }
        assert!(
            adapter
                .list_raw(with_token(ListRequest::default(), "atok"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn grpc_put_raw_is_write_scoped() {
        let adapter = fs_adapter(tomb_auth());
        let err = adapter
            .put_raw(with_token(put_req_with("origin", b"{}", None), "rtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn grpc_put_raw_by_scoped_writer_is_stamped_with_the_writer() {
        // A non-admin cannot forge authorship through PutRaw (ADR 0015).
        let adapter = fs_adapter(tomb_auth());
        let resp = adapter
            .put_raw(with_token(put_req_with("origin", b"{}", None), "wtok"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.outcome, "committed");
        assert_eq!(
            raw_record(&adapter).await.expect("stored").meta.author,
            Identity::new("writer")
        );
    }

    #[tokio::test]
    async fn grpc_put_raw_by_admin_keeps_the_incoming_author() {
        // An admin token is the replication credential.
        let adapter = fs_adapter(tomb_auth());
        let resp = adapter
            .put_raw(with_token(put_req_with("origin", b"{}", None), "atok"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.outcome, "committed");
        assert_eq!(
            raw_record(&adapter).await.expect("stored").meta.author,
            Identity::new("origin")
        );
    }

    #[tokio::test]
    async fn grpc_put_raw_in_open_mode_keeps_the_incoming_author() {
        // Open mode's implicit principal is an admin.
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        let adapter = GrpcAdapter::new(Service::new(fs.clone(), fs));
        let resp = adapter
            .put_raw(Request::new(put_req_with("origin", b"{}", None)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.outcome, "committed");
        let raw = adapter
            .get_raw(Request::new(get_req("memory")))
            .await
            .unwrap()
            .into_inner();
        let stored: Record = serde_json::from_slice(&raw.record_json).unwrap();
        assert_eq!(stored.meta.author, Identity::new("origin"));
    }

    #[tokio::test]
    async fn grpc_put_raw_over_tombstone_none_conflicts_some_overwrites() {
        let adapter = fs_adapter(tomb_auth());
        let tomb = seed_tombstone(&adapter, "wtok").await;

        let conflict = adapter
            .put_raw(with_token(put_req_with("peer", b"peer", None), "wtok"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(conflict.outcome, "conflict");

        let ok = adapter
            .put_raw(with_token(
                put_req_with("peer", b"peer", Some(tomb.revision.clone())),
                "wtok",
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(ok.outcome, "committed");
        let stored = raw_record(&adapter).await.expect("stored");
        assert_eq!(stored.revision, Revision::initial(b"peer"));
    }

    #[tokio::test]
    async fn grpc_put_not_found_is_failed_precondition() {
        let adapter = fs_adapter(tomb_auth());
        let never = Some(Revision::initial(b"never current"));
        let err = adapter
            .put_raw(with_token(put_req_with("w", b"{}", never.clone()), "wtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);

        seed_tombstone(&adapter, "wtok").await;
        let err = adapter
            .put(with_token(put_req_with("w", b"{}", never), "wtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn grpc_purge_requires_admin() {
        let adapter = fs_adapter(tomb_auth());
        let tomb = seed_tombstone(&adapter, "wtok").await;

        for token in ["rtok", "wtok"] {
            let err = adapter
                .purge(with_token(purge_req("memory", &tomb.revision), token))
                .await
                .unwrap_err();
            assert_eq!(err.code(), tonic::Code::PermissionDenied, "{token}");
        }
        let err = adapter
            .purge(Request::new(purge_req("memory", &tomb.revision)))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);

        let ok = adapter
            .purge(with_token(purge_req("memory", &tomb.revision), "atok"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(ok.outcome, "deleted");
        assert_eq!(raw_record(&adapter).await, None);
    }

    #[tokio::test]
    async fn grpc_purge_stale_expected_is_conflict() {
        let adapter = fs_adapter(tomb_auth());
        seed_tombstone(&adapter, "wtok").await;
        let resp = adapter
            .purge(with_token(
                purge_req("memory", &Revision::initial(b"not the tombstone")),
                "atok",
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.outcome, "conflict");
        let _: gonzalo_core::Conflict = serde_json::from_slice(&resp.payload_json).unwrap();
    }

    #[tokio::test]
    async fn grpc_purge_authorizes_before_parsing() {
        let adapter = fs_adapter(tomb_auth());
        let garbage = PurgeRequest {
            namespace: "memory".into(),
            collection: "col".into(),
            id: "x".into(),
            expected_json: b"not json".to_vec(),
        };
        let err = adapter
            .purge(with_token(garbage.clone(), "wtok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        let err = adapter
            .purge(with_token(garbage, "atok"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn grpc_raw_backend_error_is_opaque() {
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        let adapter = GrpcAdapter::new(Service::new(Arc::new(DownStore), fs));
        let err = adapter
            .get_raw(Request::new(get_req("any")))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Internal);
        assert_eq!(err.message(), "internal error");
    }

    // --- gRPC delete carries a claimed author (R1) ---

    #[tokio::test]
    async fn grpc_delete_by_non_admin_ignores_a_claimed_author() {
        // The live record is seeded by admin, so the prior author is "admin" —
        // distinct from both "forged" (the claim) and "writer" (the deleter).
        let adapter = fs_adapter(tomb_auth());
        adapter
            .put(with_token(put_req("memory", "admin"), "atok"))
            .await
            .unwrap();
        let mut req = delete_req("memory");
        req.author_json = serde_json::to_vec(&Identity::new("forged")).unwrap();
        let del = adapter
            .delete(with_token(req, "wtok"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(del.outcome, "deleted");
        let tomb = raw_record(&adapter).await.expect("tombstone");
        assert_eq!(tomb.meta.author, Identity::new("writer"));
    }

    #[tokio::test]
    async fn grpc_delete_by_admin_keeps_a_claimed_author() {
        let adapter = fs_adapter(tomb_auth());
        adapter
            .put(with_token(put_req("memory", "writer"), "wtok"))
            .await
            .unwrap();
        let mut req = delete_req("memory");
        req.author_json = serde_json::to_vec(&Identity::new("origin")).unwrap();
        let del = adapter
            .delete(with_token(req, "atok"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(del.outcome, "deleted");
        let tomb = raw_record(&adapter).await.expect("tombstone");
        assert_eq!(tomb.meta.author, Identity::new("origin"));
    }

    #[tokio::test]
    async fn grpc_open_mode_delete_keeps_a_claimed_author() {
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        let adapter = GrpcAdapter::new(Service::new(fs.clone(), fs));
        adapter
            .put(Request::new(put_req("memory", "writer")))
            .await
            .unwrap();
        let mut req = delete_req("memory");
        req.author_json = serde_json::to_vec(&Identity::new("origin")).unwrap();
        let del = adapter
            .delete(Request::new(req))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(del.outcome, "deleted");
        let raw = adapter
            .get_raw(Request::new(get_req("memory")))
            .await
            .unwrap()
            .into_inner();
        let tomb: Record = serde_json::from_slice(&raw.record_json).unwrap();
        assert_eq!(tomb.meta.author, Identity::new("origin"));
    }

    #[tokio::test]
    async fn grpc_delete_rejects_a_malformed_author() {
        let adapter = fs_adapter(tomb_auth());
        adapter
            .put(with_token(put_req("memory", "admin"), "atok"))
            .await
            .unwrap();
        let mut req = delete_req("memory");
        req.author_json = b"not json".to_vec();
        let err = adapter.delete(with_token(req, "atok")).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
