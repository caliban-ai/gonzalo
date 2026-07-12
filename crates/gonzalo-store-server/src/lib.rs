//! A `Store` backed by a remote gonzalo daemon. Operators choose the
//! transport: [`ServerStore::http`] (HTTP/JSON via reqwest) or
//! [`ServerStore::grpc`] (gRPC via tonic). Both speak the same JSON
//! serialization of `gonzalo-core` types. Each transport has a
//! `*_with_token` constructor that sends `Authorization: Bearer <token>`.

use async_trait::async_trait;
use gonzalo_core::{
    CoreError, DeleteResult, KeyPrefix, PutResult, Record, RecordKey, Result, Revision, Store,
    store::Conflict,
};
use gonzalo_proto::http::{DeleteBody, DeleteOutcome, PutBody, PutOutcome};
use gonzalo_proto::v1::{
    DeleteRequest, GetRequest, ListRequest, PutRequest, gonzalo_client::GonzaloClient,
};
use tonic::transport::Channel;

enum Backend {
    Http {
        base: reqwest::Url,
        client: reqwest::Client,
        token: Option<String>,
    },
    Grpc {
        client: GonzaloClient<Channel>,
        token: Option<String>,
    },
}

/// A client substrate over a remote gonzalo daemon.
pub struct ServerStore {
    backend: Backend,
}

impl ServerStore {
    /// Talk to the daemon's HTTP/JSON API rooted at `base_url`.
    pub fn http(base_url: &str) -> Result<Self> {
        Self::http_inner(base_url, None)
    }

    /// As [`ServerStore::http`], sending a bearer token on every request.
    pub fn http_with_token(base_url: &str, token: impl Into<String>) -> Result<Self> {
        Self::http_inner(base_url, Some(token.into()))
    }

    fn http_inner(base_url: &str, token: Option<String>) -> Result<Self> {
        let base = reqwest::Url::parse(base_url).map_err(|e| CoreError::Backend(e.to_string()))?;
        Ok(Self {
            backend: Backend::Http {
                base,
                client: reqwest::Client::new(),
                token,
            },
        })
    }

    /// Talk to the daemon's gRPC API at `endpoint` (e.g. `http://host:50051`).
    pub async fn grpc(endpoint: String) -> Result<Self> {
        Self::grpc_inner(endpoint, None).await
    }

    /// As [`ServerStore::grpc`], sending a bearer token on every call.
    pub async fn grpc_with_token(endpoint: String, token: impl Into<String>) -> Result<Self> {
        Self::grpc_inner(endpoint, Some(token.into())).await
    }

    async fn grpc_inner(endpoint: String, token: Option<String>) -> Result<Self> {
        let client = GonzaloClient::connect(endpoint)
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        Ok(Self {
            backend: Backend::Grpc { client, token },
        })
    }

    fn records_url(base: &reqwest::Url, key: &RecordKey) -> Result<reqwest::Url> {
        let mut url = base.clone();
        url.path_segments_mut()
            .map_err(|_| CoreError::Backend("base URL cannot be a base".into()))?
            .extend(["v1", "records", &key.namespace, &key.collection, &key.id]);
        Ok(url)
    }
}

/// Wrap a gRPC message in a request, attaching the bearer token if present.
fn grpc_request<T>(msg: T, token: &Option<String>) -> Result<tonic::Request<T>> {
    let mut req = tonic::Request::new(msg);
    if let Some(t) = token {
        let value = format!("Bearer {t}")
            .parse()
            .map_err(|_| CoreError::Backend("invalid token characters".into()))?;
        req.metadata_mut().insert("authorization", value);
    }
    Ok(req)
}

fn maybe_auth(rb: reqwest::RequestBuilder, token: &Option<String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => rb.bearer_auth(t),
        None => rb,
    }
}

#[async_trait]
impl Store for ServerStore {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::records_url(base, key)?;
                let resp = maybe_auth(client.get(url), token)
                    .send()
                    .await
                    .map_err(be)?;
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                let resp = resp.error_for_status().map_err(be)?;
                Ok(Some(resp.json::<Record>().await.map_err(be)?))
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    GetRequest {
                        namespace: key.namespace.clone(),
                        collection: key.collection.clone(),
                        id: key.id.clone(),
                    },
                    token,
                )?;
                let resp = client.get(req).await.map_err(status)?.into_inner();
                if resp.found {
                    Ok(Some(serde_json::from_slice(&resp.record_json).map_err(se)?))
                } else {
                    Ok(None)
                }
            }
        }
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::records_url(base, &record.key)?;
                let body = PutBody { record, expected };
                let resp = maybe_auth(client.put(url).json(&body), token)
                    .send()
                    .await
                    .map_err(be)?;
                // Read the body as text first so error statuses (403/413/400,
                // which carry plain-text bodies) surface their real status and
                // message instead of being masked as a JSON decode error (#147).
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_put_response(status, &text)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    PutRequest {
                        record_json: serde_json::to_vec(&record).map_err(se)?,
                        expected_json: serde_json::to_vec(&expected).map_err(se)?,
                    },
                    token,
                )?;
                let resp = client.put(req).await.map_err(status)?.into_inner();
                match resp.outcome.as_str() {
                    "committed" => {
                        let rev: Revision =
                            serde_json::from_slice(&resp.payload_json).map_err(se)?;
                        Ok(PutResult::Committed(rev))
                    }
                    "conflict" => {
                        let c: Conflict = serde_json::from_slice(&resp.payload_json).map_err(se)?;
                        Ok(PutResult::Conflict(Box::new(c)))
                    }
                    other => Err(CoreError::Backend(format!("unknown put outcome: {other}"))),
                }
            }
        }
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let mut url = base.clone();
                url.path_segments_mut()
                    .map_err(|_| CoreError::Backend("base URL cannot be a base".into()))?
                    .extend(["v1", "keys"]);
                {
                    let mut q = url.query_pairs_mut();
                    if let Some(ns) = &prefix.namespace {
                        q.append_pair("namespace", ns);
                    }
                    if let Some(col) = &prefix.collection {
                        q.append_pair("collection", col);
                    }
                }
                let resp = maybe_auth(client.get(url), token)
                    .send()
                    .await
                    .map_err(be)?
                    .error_for_status()
                    .map_err(be)?;
                Ok(resp.json::<Vec<RecordKey>>().await.map_err(be)?)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    ListRequest {
                        namespace: prefix.namespace.clone(),
                        collection: prefix.collection.clone(),
                    },
                    token,
                )?;
                let resp = client.list(req).await.map_err(status)?.into_inner();
                resp.keys_json
                    .iter()
                    .map(|b| serde_json::from_slice::<RecordKey>(b).map_err(se))
                    .collect()
            }
        }
    }

    async fn delete(&self, key: &RecordKey, expected: Option<Revision>) -> Result<DeleteResult> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::records_url(base, key)?;
                let body = DeleteBody { expected };
                let resp = maybe_auth(client.delete(url).json(&body), token)
                    .send()
                    .await
                    .map_err(be)?;
                // Read the body as text first so error statuses (403/400, which
                // carry plain-text bodies) surface their real status and message
                // instead of being masked as a JSON decode error (mirrors `put`).
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_delete_response(status, &text)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    DeleteRequest {
                        namespace: key.namespace.clone(),
                        collection: key.collection.clone(),
                        id: key.id.clone(),
                        expected_json: serde_json::to_vec(&expected).map_err(se)?,
                    },
                    token,
                )?;
                let resp = client.delete(req).await.map_err(status)?.into_inner();
                match resp.outcome.as_str() {
                    "deleted" => Ok(DeleteResult::Deleted),
                    "conflict" => {
                        let c: Conflict = serde_json::from_slice(&resp.payload_json).map_err(se)?;
                        Ok(DeleteResult::Conflict(Box::new(c)))
                    }
                    other => Err(CoreError::Backend(format!(
                        "unknown delete outcome: {other}"
                    ))),
                }
            }
        }
    }
}

fn delete_outcome_to_result(outcome: DeleteOutcome) -> DeleteResult {
    match outcome {
        DeleteOutcome::Deleted => DeleteResult::Deleted,
        DeleteOutcome::Conflict { conflict } => DeleteResult::Conflict(conflict),
    }
}

/// Decide a `delete`'s result from the HTTP response status and body text.
///
/// The daemon speaks JSON (`DeleteOutcome`) only for `200 OK` (deleted) and
/// `409 Conflict`; every other status (`403` authorization denial, `400` bad
/// request) carries a plain-text body, surfaced verbatim as
/// `CoreError::Backend("daemon returned <status>: <body>")` (mirrors `put`).
fn classify_delete_response(status: reqwest::StatusCode, body: &str) -> Result<DeleteResult> {
    match status {
        reqwest::StatusCode::OK | reqwest::StatusCode::CONFLICT => {
            let outcome: DeleteOutcome = serde_json::from_str(body).map_err(se)?;
            Ok(delete_outcome_to_result(outcome))
        }
        other => Err(CoreError::Backend(format!(
            "daemon returned {other}: {body}"
        ))),
    }
}

fn outcome_to_result(outcome: PutOutcome) -> PutResult {
    match outcome {
        PutOutcome::Committed { revision } => PutResult::Committed(revision),
        PutOutcome::Conflict { conflict } => PutResult::Conflict(conflict),
    }
}

/// Decide a `put`'s result from the HTTP response status and body text.
///
/// The daemon speaks JSON (`PutOutcome`) only for `200 OK` (committed) and
/// `409 Conflict`; every other status — notably `403` (namespace authorization
/// denial), `413` (too large), and `400` (bad request) — carries a plain-text
/// body. Feeding those bodies to a JSON parser masks the real failure behind a
/// generic "error decoding response body" (#147), so any other status is
/// surfaced verbatim as `CoreError::Backend("daemon returned <status>: <body>")`.
fn classify_put_response(status: reqwest::StatusCode, body: &str) -> Result<PutResult> {
    match status {
        reqwest::StatusCode::OK | reqwest::StatusCode::CONFLICT => {
            let outcome: PutOutcome = serde_json::from_str(body).map_err(se)?;
            Ok(outcome_to_result(outcome))
        }
        other => Err(CoreError::Backend(format!(
            "daemon returned {other}: {body}"
        ))),
    }
}

fn be<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Backend(e.to_string())
}
fn se<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Serde(e.to_string())
}
fn status(s: tonic::Status) -> CoreError {
    CoreError::Backend(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_core::store::Conflict;
    use gonzalo_core::{Body, Identity, Meta, Record, RecordKind};
    use reqwest::StatusCode;
    use std::collections::BTreeMap;

    fn sample_record() -> Record {
        let body = Body::Inline(b"hello".to_vec());
        Record {
            key: RecordKey::new("ns", "col", "id"),
            kind: RecordKind::Topic,
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            meta: Meta {
                author: Identity::new("tester"),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
        }
    }

    /// `200 OK` carries a `committed` JSON body → `PutResult::Committed`.
    #[test]
    fn ok_body_parses_committed() {
        let revision = Revision::initial(b"hello");
        let json = serde_json::to_string(&PutOutcome::Committed {
            revision: revision.clone(),
        })
        .unwrap();
        let result = classify_put_response(StatusCode::OK, &json).unwrap();
        assert!(matches!(result, PutResult::Committed(r) if r == revision));
    }

    /// `409 Conflict` carries a `conflict` JSON body → `PutResult::Conflict`.
    #[test]
    fn conflict_body_parses_conflict() {
        let record = sample_record();
        let conflict = Conflict {
            key: record.key.clone(),
            expected: None,
            current: record,
        };
        let json = serde_json::to_string(&PutOutcome::Conflict {
            conflict: Box::new(conflict),
        })
        .unwrap();
        let result = classify_put_response(StatusCode::CONFLICT, &json).unwrap();
        assert!(matches!(result, PutResult::Conflict(_)));
    }

    /// #147: a `403` with a plain-text body surfaces the status + body as a
    /// `Backend` error — NOT a masked JSON decode error.
    #[test]
    fn forbidden_surfaces_status_and_body() {
        let body = "principal \"alice\" lacks Write on namespace \"secrets\"";
        let err = classify_put_response(StatusCode::FORBIDDEN, body).unwrap_err();
        match err {
            CoreError::Backend(msg) => {
                assert!(msg.contains("403"), "want status 403 in {msg:?}");
                assert!(msg.contains(body), "want daemon body in {msg:?}");
                assert!(
                    !msg.contains("decoding"),
                    "must not be a decode error: {msg:?}"
                );
            }
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    /// `413 Payload Too Large` (plain-text body) also surfaces status + body.
    #[test]
    fn payload_too_large_surfaces_status_and_body() {
        let body = "record exceeds max size";
        let err = classify_put_response(StatusCode::PAYLOAD_TOO_LARGE, body).unwrap_err();
        match err {
            CoreError::Backend(msg) => {
                assert!(msg.contains("413"), "want status 413 in {msg:?}");
                assert!(msg.contains(body), "want daemon body in {msg:?}");
            }
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    /// `400 Bad Request` (plain-text body) surfaces status + body, not a decode
    /// error.
    #[test]
    fn bad_request_surfaces_status_and_body() {
        let body = "path/body key disagreement";
        let err = classify_put_response(StatusCode::BAD_REQUEST, body).unwrap_err();
        match err {
            CoreError::Backend(msg) => {
                assert!(msg.contains("400"), "want status 400 in {msg:?}");
                assert!(msg.contains(body), "want daemon body in {msg:?}");
            }
            other => panic!("expected Backend error, got {other:?}"),
        }
    }
}
