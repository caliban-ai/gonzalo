//! gRPC transport: adapts the generated `Gonzalo` service to the shared
//! `Service`, carrying `gonzalo-core` types as JSON payloads.

use crate::Service;
use gonzalo_core::{KeyPrefix, PutResult, Record, RecordKey, Revision};
use gonzalo_proto::v1::{
    GetRequest, GetResponse, ListRequest, ListResponse, PutRequest, PutResponse, TicketSyncRequest,
    TicketSyncResponse,
    gonzalo_server::{Gonzalo, GonzaloServer},
};
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
        let conn: gonzalo_ticket_config::Connection =
            serde_json::from_slice(&r.connection_json).map_err(internal)?;
        let summary = self
            .service
            .ticket_sync(&conn, "gonzalod")
            .await
            .map_err(Status::internal)?;
        Ok(Response::new(TicketSyncResponse {
            imported: summary.imported as u64,
            updated: summary.updated as u64,
            unchanged: summary.unchanged as u64,
        }))
    }
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
