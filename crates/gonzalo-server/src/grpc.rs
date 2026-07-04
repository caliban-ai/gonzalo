//! gRPC transport: adapts the generated `Gonzalo` service to the shared
//! `Service`, carrying `gonzalo-core` types as JSON payloads.

use crate::Service;
use gonzalo_core::{KeyPrefix, PutResult, Record, RecordKey, Revision};
use gonzalo_proto::v1::{
    GetRequest, GetResponse, GraphLocatedResponse, GraphNamesResponse, GraphQueryRequest,
    ListRequest, ListResponse, PutRequest, PutResponse, TicketSyncRequest, TicketSyncResponse,
    gonzalo_server::{Gonzalo, GonzaloServer},
};
use serde::Serialize;
use tonic::{Request, Response, Status};

/// Adapts [`Service`] to the generated gRPC trait.
pub struct GrpcAdapter {
    service: Service,
}

impl GrpcAdapter {
    pub fn new(service: Service) -> Self {
        Self { service }
    }
}

fn internal<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

#[tonic::async_trait]
impl Gonzalo for GrpcAdapter {
    async fn get(&self, req: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let r = req.into_inner();
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
        let r = req.into_inner();
        let record: Record = serde_json::from_slice(&r.record_json).map_err(internal)?;
        let expected: Option<Revision> =
            serde_json::from_slice(&r.expected_json).map_err(internal)?;
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
        let r = req.into_inner();
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
        let r = req.into_inner();
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
        let r = req.into_inner();
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
        let r = req.into_inner();
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
        let r = req.into_inner();
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
        let r = req.into_inner();
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
        let r = req.into_inner();
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

/// Serve gRPC on an already-bound listener until the process ends. When
/// `auth` is `Some`, every call must carry `authorization: Bearer <token>`.
// The interceptor must return `Result<_, tonic::Status>`; `Status` is large
// but its type is fixed by tonic's API, so the large-err lint can't be acted on.
#[allow(clippy::result_large_err)]
pub async fn serve_grpc(
    listener: tokio::net::TcpListener,
    service: Service,
    auth: Option<String>,
) -> Result<(), tonic::transport::Error> {
    let adapter = GrpcAdapter::new(service);
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    match auth {
        Some(token) => {
            let intercepted = GonzaloServer::with_interceptor(
                adapter,
                move |req: Request<()>| -> Result<Request<()>, Status> {
                    let ok = req
                        .metadata()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.strip_prefix("Bearer "))
                        .map(|t| t == token)
                        .unwrap_or(false);
                    if ok {
                        Ok(req)
                    } else {
                        Err(Status::unauthenticated("invalid or missing token"))
                    }
                },
            );
            tonic::transport::Server::builder()
                .add_service(intercepted)
                .serve_with_incoming(incoming)
                .await
        }
        None => {
            tonic::transport::Server::builder()
                .add_service(GonzaloServer::new(adapter))
                .serve_with_incoming(incoming)
                .await
        }
    }
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
