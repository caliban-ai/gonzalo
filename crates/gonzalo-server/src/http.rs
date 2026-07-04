//! HTTP/JSON transport over the shared `Service`, using axum.

use crate::Service;
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{Next, from_fn},
    response::{IntoResponse, Response},
    routing::get,
};
use gonzalo_core::{KeyPrefix, PutResult, RecordKey};
use gonzalo_proto::http::{PutBody, PutOutcome};
use serde::Deserialize;
use std::sync::Arc;

/// Build the axum router. When `auth` is `Some`, every request must carry
/// `Authorization: Bearer <token>`.
pub fn router(service: Service, auth: Option<String>) -> Router {
    let mut app = Router::new()
        .route(
            "/v1/records/{ns}/{col}/{id}",
            get(get_record).put(put_record),
        )
        .route("/v1/keys", get(list_keys))
        .route("/v1/tickets/sync", axum::routing::post(ticket_sync))
        .route("/v1/graph/definitions", get(graph_definitions))
        .route("/v1/graph/references", get(graph_references_to))
        .route("/v1/graph/callers", get(graph_callers_of))
        .route("/v1/graph/callees", get(graph_callees))
        .route("/v1/graph/impact", get(graph_impact))
        .with_state(Arc::new(service));
    if let Some(token) = auth {
        let token = Arc::new(token);
        app = app.layer(from_fn(move |req: Request, next: Next| {
            let token = token.clone();
            async move {
                let ok = bearer(req.headers())
                    .map(|t| t == token.as_str())
                    .unwrap_or(false);
                if ok {
                    next.run(req).await
                } else {
                    StatusCode::UNAUTHORIZED.into_response()
                }
            }
        }));
    }
    app
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn server_error<E: std::fmt::Display>(e: E) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

async fn get_record(
    State(svc): State<Arc<Service>>,
    Path((ns, col, id)): Path<(String, String, String)>,
) -> Response {
    match svc.get(&RecordKey::new(ns, col, id)).await {
        Ok(Some(rec)) => (StatusCode::OK, Json(rec)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => server_error(e),
    }
}

async fn put_record(State(svc): State<Arc<Service>>, Json(body): Json<PutBody>) -> Response {
    match svc.put(body.record, body.expected).await {
        Ok(PutResult::Committed(revision)) => {
            (StatusCode::OK, Json(PutOutcome::Committed { revision })).into_response()
        }
        Ok(PutResult::Conflict(conflict)) => (
            StatusCode::CONFLICT,
            Json(PutOutcome::Conflict { conflict }),
        )
            .into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
struct ListQuery {
    namespace: Option<String>,
    collection: Option<String>,
}

async fn list_keys(State(svc): State<Arc<Service>>, Query(q): Query<ListQuery>) -> Response {
    let prefix = KeyPrefix {
        namespace: q.namespace,
        collection: q.collection,
    };
    match svc.list(&prefix).await {
        Ok(keys) => (StatusCode::OK, Json(keys)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn ticket_sync(
    State(svc): State<Arc<Service>>,
    Json(conn): Json<gonzalo_ticket_config::Connection>,
) -> Response {
    match svc.ticket_sync(&conn, "gonzalod").await {
        Ok(summary) => (StatusCode::OK, Json(summary)).into_response(),
        Err(crate::service::TicketSyncError::BadRequest(m)) => {
            (StatusCode::BAD_REQUEST, m).into_response()
        }
        Err(crate::service::TicketSyncError::Internal(m)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, m).into_response()
        }
    }
}

/// Selects a code-graph view `(repo, view)` and the `name` a query is about,
/// e.g. `GET /v1/graph/impact?repo=acme/widgets&view=main&name=helper`.
#[derive(Deserialize)]
struct GraphQuery {
    repo: String,
    view: String,
    name: String,
}

async fn graph_definitions(
    State(svc): State<Arc<Service>>,
    Query(q): Query<GraphQuery>,
) -> Response {
    match svc.graph_definitions(&q.repo, &q.view, &q.name).await {
        Ok(items) => (StatusCode::OK, Json(items)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn graph_references_to(
    State(svc): State<Arc<Service>>,
    Query(q): Query<GraphQuery>,
) -> Response {
    match svc.graph_references_to(&q.repo, &q.view, &q.name).await {
        Ok(items) => (StatusCode::OK, Json(items)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn graph_callers_of(
    State(svc): State<Arc<Service>>,
    Query(q): Query<GraphQuery>,
) -> Response {
    match svc.graph_callers_of(&q.repo, &q.view, &q.name).await {
        Ok(names) => (StatusCode::OK, Json(names)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn graph_callees(State(svc): State<Arc<Service>>, Query(q): Query<GraphQuery>) -> Response {
    match svc.graph_callees(&q.repo, &q.view, &q.name).await {
        Ok(names) => (StatusCode::OK, Json(names)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn graph_impact(State(svc): State<Arc<Service>>, Query(q): Query<GraphQuery>) -> Response {
    match svc.graph_impact(&q.repo, &q.view, &q.name).await {
        Ok(names) => (StatusCode::OK, Json(names)).into_response(),
        Err(e) => server_error(e),
    }
}

/// Serve HTTP/JSON on an already-bound listener until the process ends.
pub async fn serve_http(
    listener: tokio::net::TcpListener,
    service: Service,
    auth: Option<String>,
) -> std::io::Result<()> {
    axum::serve(listener, router(service, auth)).await
}
