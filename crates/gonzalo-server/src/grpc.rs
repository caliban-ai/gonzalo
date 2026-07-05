//! gRPC transport: adapts the generated `Gonzalo` service to the shared
//! `Service`, carrying `gonzalo-core` types as JSON payloads.

use crate::Service;
use crate::auth::{Access, Auth, Principal};
use gonzalo_core::{Identity, KeyPrefix, PutResult, Record, RecordKey, Revision};
use gonzalo_proto::v1::{
    GetRequest, GetResponse, GraphLocatedResponse, GraphNamesResponse, GraphQueryRequest,
    ListRequest, ListResponse, PutRequest, PutResponse, TicketSyncRequest, TicketSyncResponse,
    gonzalo_server::{Gonzalo, GonzaloServer},
};
use serde::Serialize;
use std::sync::Arc;
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};

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
        let principal = self
            .auth
            .authenticate(bearer(metadata))
            .ok_or_else(|| Status::unauthenticated("invalid or missing token"))?;
        if principal.allows(access, namespace) {
            Ok(principal)
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

fn internal<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
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
        let mut record: Record = serde_json::from_slice(&r.record_json).map_err(internal)?;
        let expected: Option<Revision> =
            serde_json::from_slice(&r.expected_json).map_err(internal)?;
        let principal = self.authorize(&metadata, Access::Write, &record.key.namespace)?;
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
                crate::service::TicketSyncError::BadRequest(m) => Status::invalid_argument(m),
                crate::service::TicketSyncError::Internal(m) => Status::internal(m),
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
    let adapter = GrpcAdapter::with_auth(service, auth);
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tonic::transport::Server::builder()
        .add_service(GonzaloServer::new(adapter))
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
