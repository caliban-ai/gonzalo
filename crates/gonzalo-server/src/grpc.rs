//! gRPC transport: adapts the generated `Gonzalo` service to the shared
//! `Service`, carrying `gonzalo-core` types as JSON payloads.

use crate::Service;
use crate::auth::{Access, Auth, Principal};
use gonzalo_core::{
    ContentHash, DeleteResult, Identity, KeyPrefix, PutResult, Record, RecordKey, Revision,
};
use gonzalo_proto::v1::{
    DeleteBlobRequest, DeleteBlobResponse, DeleteRequest, DeleteResponse, GetBlobRequest,
    GetBlobResponse, GetRequest, GetResponse, GraphLocatedResponse, GraphNamesResponse,
    GraphQueryRequest, ListBlobsRequest, ListBlobsResponse, ListRequest, ListResponse,
    PutBlobRequest, PutBlobResponse, PutRequest, PutResponse, TicketSyncRequest,
    TicketSyncResponse,
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
        let resp = match rec {
            Some(rec) => GetResponse {
                found: true,
                record_json: serde_json::to_vec(&rec).map_err(internal)?,
            },
            None => GetResponse {
                found: false,
                record_json: Vec::new(),
            },
        };
        Ok(Response::new(resp))
    }

    async fn put(&self, req: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Authenticate BEFORE deserializing attacker-controlled JSON (#146): an
        // unauthenticated caller is rejected without ever feeding its body to
        // serde. Only then parse the body (malformed input is the caller's
        // error → invalid_argument, not internal) and authorize the write
        // against the namespace named in the record's key.
        let principal = self.authenticate(&metadata)?;
        let mut record: Record = serde_json::from_slice(&r.record_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let expected: Option<Revision> = serde_json::from_slice(&r.expected_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.check_access(&principal, Access::Write, &record.key.namespace)?;
        // Stamp the author from the authenticated principal — unforgeable (ADR
        // 0015). Open mode (no auth) leaves the record's author untouched.
        if principal.is_authenticated() {
            record.meta.author = Identity::new(principal.name());
        }
        let outcome = self.service.put(record, expected).await.map_err(internal)?;
        let resp = match outcome {
            PutResult::Committed(rev) => PutResponse {
                outcome: "committed".into(),
                payload_json: serde_json::to_vec(&rev).map_err(internal)?,
            },
            PutResult::Conflict(c) => PutResponse {
                outcome: "conflict".into(),
                payload_json: serde_json::to_vec(&*c).map_err(internal)?,
            },
        };
        Ok(Response::new(resp))
    }

    async fn delete(
        &self,
        req: Request<DeleteRequest>,
    ) -> Result<Response<DeleteResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Authenticate BEFORE deserializing attacker-controlled JSON (#146), then
        // parse the precondition (malformed input is the caller's error →
        // invalid_argument), authorize the write against the path's namespace,
        // and build the key from (namespace, collection, id).
        let principal = self.authenticate(&metadata)?;
        let expected: Option<Revision> = serde_json::from_slice(&r.expected_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.check_access(&principal, Access::Write, &r.namespace)?;
        let key = RecordKey::new(r.namespace, r.collection, r.id);
        let outcome = self
            .service
            .delete(&key, expected)
            .await
            .map_err(internal)?;
        let resp = match outcome {
            DeleteResult::Deleted => DeleteResponse {
                outcome: "deleted".into(),
                payload_json: Vec::new(),
            },
            DeleteResult::Conflict(c) => DeleteResponse {
                outcome: "conflict".into(),
                payload_json: serde_json::to_vec(&*c).map_err(internal)?,
            },
        };
        Ok(Response::new(resp))
    }

    async fn list(&self, req: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        // Listing without a namespace spans all namespaces → requires admin
        // (`read` on `"*"`); a namespaced list needs read on that namespace.
        self.authorize(
            &metadata,
            Access::Read,
            r.namespace.as_deref().unwrap_or("*"),
        )?;
        let prefix = KeyPrefix {
            namespace: r.namespace,
            collection: r.collection,
        };
        let keys = self.service.list(&prefix).await.map_err(internal)?;
        let keys_json = keys
            .iter()
            .map(serde_json::to_vec)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(internal)?;
        Ok(Response::new(ListResponse { keys_json }))
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
            .graph_impact(&r.repo, &r.view_id, &r.name)
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
pub async fn serve_grpc(
    listener: tokio::net::TcpListener,
    service: Service,
    auth: Arc<Auth>,
) -> Result<(), tonic::transport::Error> {
    let max_blob = service.max_blob_size();
    let adapter = GrpcAdapter::with_auth(service, auth);
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tonic::transport::Server::builder()
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
            Err(gonzalo_core::CoreError::Backend(
                "s3://secret-bucket".into(),
            ))
        }
        async fn list(
            &self,
            _prefix: &gonzalo_core::KeyPrefix,
        ) -> gonzalo_core::Result<Vec<RecordKey>> {
            Err(gonzalo_core::CoreError::Backend(
                "s3://secret-bucket".into(),
            ))
        }
        async fn delete(
            &self,
            _key: &RecordKey,
            _expected: Option<Revision>,
        ) -> gonzalo_core::Result<DeleteResult> {
            Err(gonzalo_core::CoreError::Backend(
                "s3://secret-bucket".into(),
            ))
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
}
