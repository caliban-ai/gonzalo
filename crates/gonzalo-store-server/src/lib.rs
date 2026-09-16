//! A `Store` backed by a remote gonzalo daemon. Operators choose the
//! transport: [`ServerStore::http`] (HTTP/JSON via reqwest) or
//! [`ServerStore::grpc`] (gRPC via tonic). Both speak the same JSON
//! serialization of `gonzalo-core` types. Each transport has a
//! `*_with_token` constructor that sends `Authorization: Bearer <token>`.

use async_trait::async_trait;
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DeleteResult, Identity, KeyPrefix, PutResult, Record,
    RecordKey, Result, Revision, Store, store::Conflict,
};
use gonzalo_proto::http::{
    DeleteBody, DeleteOutcome, PurgeBody, PutBody, PutOutcome, RawRecordBody,
};
use gonzalo_proto::v1::{
    DeleteBlobRequest, DeleteRequest, GetBlobRequest, GetRequest, ListBlobsRequest, ListRequest,
    ListResponse, PurgeRequest, PutBlobRequest, PutRequest, PutResponse,
    gonzalo_client::GonzaloClient,
};
use tonic::transport::Channel;

/// The error every replication call (`get_raw`, `list_raw`, `put_raw`,
/// `purge`) returns against a daemon without the replication surface: HTTP
/// `404` on the route, or gRPC `Unimplemented`. There is deliberately **no
/// fallback** to the consumer routes. Consumer reads hide tombstones and
/// consumer put re-stamps recreations, so replicating through them resurrects
/// deleted records (spec §3.6).
pub const DAEMON_PREDATES_REPLICATION: &str =
    "daemon predates replication reads (gonzalo#203); upgrade gonzalod";

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
            .map_err(|e| CoreError::Backend(e.to_string()))?
            .max_decoding_message_size(gonzalo_proto::DEFAULT_MAX_BLOB_SIZE);
        Ok(Self {
            backend: Backend::Grpc { client, token },
        })
    }

    /// `base` + `segments` + the record key's three segments.
    fn key_url(base: &reqwest::Url, segments: &[&str], key: &RecordKey) -> Result<reqwest::Url> {
        let mut url = base.clone();
        url.path_segments_mut()
            .map_err(|_| CoreError::Backend("base URL cannot be a base".into()))?
            .extend(segments)
            .extend([&key.namespace, &key.collection, &key.id]);
        Ok(url)
    }

    fn records_url(base: &reqwest::Url, key: &RecordKey) -> Result<reqwest::Url> {
        Self::key_url(base, &["v1", "records"], key)
    }

    /// `…/v1/raw/records/{ns}/{col}/{id}` (gonzalo#203).
    fn raw_records_url(base: &reqwest::Url, key: &RecordKey) -> Result<reqwest::Url> {
        Self::key_url(base, &["v1", "raw", "records"], key)
    }

    /// `…/v1/purge/{ns}/{col}/{id}` (gonzalo#203).
    fn purge_url(base: &reqwest::Url, key: &RecordKey) -> Result<reqwest::Url> {
        Self::key_url(base, &["v1", "purge"], key)
    }

    /// `base` + `segments` + `?namespace=&collection=` from `prefix` (shared by
    /// `/v1/keys` and `/v1/raw/keys`).
    fn keys_url(
        base: &reqwest::Url,
        segments: &[&str],
        prefix: &KeyPrefix,
    ) -> Result<reqwest::Url> {
        let mut url = base.clone();
        url.path_segments_mut()
            .map_err(|_| CoreError::Backend("base URL cannot be a base".into()))?
            .extend(segments);
        {
            let mut q = url.query_pairs_mut();
            if let Some(ns) = &prefix.namespace {
                q.append_pair("namespace", ns);
            }
            if let Some(col) = &prefix.collection {
                q.append_pair("collection", col);
            }
        }
        Ok(url)
    }

    /// `…/v1/blobs` (list) or `…/v1/blobs/{hash}` (one blob) when `hash` is set.
    fn blobs_url(base: &reqwest::Url, hash: Option<&str>) -> Result<reqwest::Url> {
        let mut url = base.clone();
        {
            let mut seg = url
                .path_segments_mut()
                .map_err(|_| CoreError::Backend("base URL cannot be a base".into()))?;
            seg.extend(["v1", "blobs"]);
            if let Some(h) = hash {
                seg.push(h);
            }
        }
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
                let resp = ensure_read_ok(resp).await?;
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
                decode_get_response(resp)
            }
        }
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        let key = record.key.clone();
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::records_url(base, &key)?;
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
                classify_put_response_for(&key, status, &text)
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
                let resp = client
                    .put(req)
                    .await
                    .map_err(|s| put_status(s, &key))?
                    .into_inner();
                decode_put_response(resp)
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
                let url = Self::keys_url(base, &["v1", "keys"], prefix)?;
                let resp = maybe_auth(client.get(url), token)
                    .send()
                    .await
                    .map_err(be)?;
                let resp = ensure_read_ok(resp).await?;
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
                decode_keys(resp)
            }
        }
    }

    /// Sends the delete with its author. The daemon honours a named deleter
    /// only for an admin credential or open mode; a non-admin token is always
    /// stamped as itself (ADR 0015, gonzalo#203).
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::records_url(base, key)?;
                let body = DeleteBody { expected, author };
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
                let author_json = match &author {
                    None => Vec::new(),
                    Some(a) => serde_json::to_vec(a).map_err(se)?,
                };
                let req = grpc_request(
                    DeleteRequest {
                        namespace: key.namespace.clone(),
                        collection: key.collection.clone(),
                        id: key.id.clone(),
                        expected_json: serde_json::to_vec(&expected).map_err(se)?,
                        author_json,
                    },
                    token,
                )?;
                let resp = client.delete(req).await.map_err(status)?.into_inner();
                decode_delete_outcome(&resp.outcome, &resp.payload_json)
            }
        }
    }

    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::raw_records_url(base, key)?;
                let resp = maybe_auth(client.get(url), token)
                    .send()
                    .await
                    .map_err(be)?;
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_raw_get_response(status, &text)
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
                let resp = client
                    .get_raw(req)
                    .await
                    .map_err(replication_status)?
                    .into_inner();
                decode_get_response(resp)
            }
        }
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::keys_url(base, &["v1", "raw", "keys"], prefix)?;
                let resp = maybe_auth(client.get(url), token)
                    .send()
                    .await
                    .map_err(be)?;
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_raw_list_response(status, &text)
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
                let resp = client
                    .list_raw(req)
                    .await
                    .map_err(replication_status)?
                    .into_inner();
                decode_keys(resp)
            }
        }
    }

    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        let key = record.key.clone();
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::raw_records_url(base, &key)?;
                let body = PutBody { record, expected };
                let resp = maybe_auth(client.put(url).json(&body), token)
                    .send()
                    .await
                    .map_err(be)?;
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_raw_put_response(&key, status, &text)
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
                let resp = client
                    .put_raw(req)
                    .await
                    .map_err(|s| raw_put_status(s, &key))?
                    .into_inner();
                decode_put_response(resp)
            }
        }
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::purge_url(base, key)?;
                let body = PurgeBody { expected };
                let resp = maybe_auth(client.post(url).json(&body), token)
                    .send()
                    .await
                    .map_err(be)?;
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_purge_response(status, &text)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    PurgeRequest {
                        namespace: key.namespace.clone(),
                        collection: key.collection.clone(),
                        id: key.id.clone(),
                        expected_json: serde_json::to_vec(&expected).map_err(se)?,
                    },
                    token,
                )?;
                let resp = client
                    .purge(req)
                    .await
                    .map_err(replication_status)?
                    .into_inner();
                decode_delete_outcome(&resp.outcome, &resp.payload_json)
            }
        }
    }
}

#[async_trait]
impl BlobStore for ServerStore {
    async fn put_blob(&self, content: &[u8]) -> Result<ContentHash> {
        // The blob is content-addressed: compute the hash locally to address
        // the request, exactly as the daemon will recompute and verify it.
        let hash = ContentHash::of(content);
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::blobs_url(base, Some(&hash.0))?;
                let resp = maybe_auth(client.put(url).body(content.to_vec()), token)
                    .send()
                    .await
                    .map_err(be)?;
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_blob_put_response(status, &text, hash)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    PutBlobRequest {
                        hash: hash.0.clone(),
                        content: content.to_vec(),
                    },
                    token,
                )?;
                let resp = client.put_blob(req).await.map_err(status)?.into_inner();
                Ok(ContentHash(resp.hash))
            }
        }
    }

    async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::blobs_url(base, Some(&hash.0))?;
                let resp = maybe_auth(client.get(url), token)
                    .send()
                    .await
                    .map_err(be)?;
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                let resp = ensure_read_ok(resp).await?;
                Ok(Some(resp.bytes().await.map_err(be)?.to_vec()))
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    GetBlobRequest {
                        hash: hash.0.clone(),
                    },
                    token,
                )?;
                let resp = client.get_blob(req).await.map_err(status)?.into_inner();
                Ok(resp.found.then_some(resp.content))
            }
        }
    }

    async fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::blobs_url(base, None)?;
                let resp = maybe_auth(client.get(url), token)
                    .send()
                    .await
                    .map_err(be)?;
                let resp = ensure_read_ok(resp).await?;
                Ok(resp.json::<Vec<ContentHash>>().await.map_err(be)?)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(ListBlobsRequest {}, token)?;
                let resp = client.list_blobs(req).await.map_err(status)?.into_inner();
                Ok(resp.hashes.into_iter().map(ContentHash).collect())
            }
        }
    }

    async fn delete_blob(&self, hash: &ContentHash) -> Result<()> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::blobs_url(base, Some(&hash.0))?;
                let resp = maybe_auth(client.delete(url), token)
                    .send()
                    .await
                    .map_err(be)?;
                let status = resp.status();
                if status == reqwest::StatusCode::OK {
                    return Ok(());
                }
                let text = resp.text().await.map_err(be)?;
                Err(CoreError::Backend(format!(
                    "daemon returned {status}: {text}"
                )))
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    DeleteBlobRequest {
                        hash: hash.0.clone(),
                    },
                    token,
                )?;
                client.delete_blob(req).await.map_err(status)?;
                Ok(())
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
        // A `400` is always the call being wrong — a consumer put of a
        // tombstone, or a path/body key disagreement (#158) — never the store
        // failing. Restore it as `Invalid` so a store behind the daemon fails
        // the way a local one does instead of looking like an outage, keeping
        // the status in the message as every other failure does (#299).
        reqwest::StatusCode::BAD_REQUEST => Err(CoreError::Invalid(format!(
            "daemon returned {}: {body}",
            reqwest::StatusCode::BAD_REQUEST
        ))),
        other => Err(CoreError::Backend(format!(
            "daemon returned {other}: {body}"
        ))),
    }
}

/// [`classify_put_response`], plus `412 Precondition Failed`: the daemon's
/// `CoreError::NotFound` (`expected` names a revision the store does not hold),
/// restored as `NotFound(key)`.
fn classify_put_response_for(
    key: &RecordKey,
    status: reqwest::StatusCode,
    body: &str,
) -> Result<PutResult> {
    match status {
        reqwest::StatusCode::PRECONDITION_FAILED => Err(CoreError::NotFound(key.clone())),
        other => classify_put_response(other, body),
    }
}

/// A raw put's result: `404` means the route does not exist →
/// [`upgrade_required`], never a fallback; otherwise as a consumer put.
fn classify_raw_put_response(
    key: &RecordKey,
    status: reqwest::StatusCode,
    body: &str,
) -> Result<PutResult> {
    match status {
        reqwest::StatusCode::NOT_FOUND => Err(upgrade_required()),
        other => classify_put_response_for(key, other, body),
    }
}

/// Decode a gRPC `PutResponse` (shared by `Put` and `PutRaw`).
fn decode_put_response(resp: PutResponse) -> Result<PutResult> {
    match resp.outcome.as_str() {
        "committed" => {
            let rev: Revision = serde_json::from_slice(&resp.payload_json).map_err(se)?;
            Ok(PutResult::Committed(rev))
        }
        "conflict" => {
            let c: Conflict = serde_json::from_slice(&resp.payload_json).map_err(se)?;
            Ok(PutResult::Conflict(Box::new(c)))
        }
        other => Err(CoreError::Backend(format!("unknown put outcome: {other}"))),
    }
}

/// Decode a gRPC `GetResponse` (shared by `Get` and `GetRaw`).
fn decode_get_response(resp: gonzalo_proto::v1::GetResponse) -> Result<Option<Record>> {
    if resp.found {
        Ok(Some(serde_json::from_slice(&resp.record_json).map_err(se)?))
    } else {
        Ok(None)
    }
}

/// Decode a gRPC delete-shaped outcome (shared by `Delete` and `Purge`).
fn decode_delete_outcome(outcome: &str, payload_json: &[u8]) -> Result<DeleteResult> {
    match outcome {
        "deleted" => Ok(DeleteResult::Deleted),
        "conflict" => {
            let c: Conflict = serde_json::from_slice(payload_json).map_err(se)?;
            Ok(DeleteResult::Conflict(Box::new(c)))
        }
        other => Err(CoreError::Backend(format!(
            "unknown delete outcome: {other}"
        ))),
    }
}

/// [`DAEMON_PREDATES_REPLICATION`] as a `CoreError`.
fn upgrade_required() -> CoreError {
    CoreError::Backend(DAEMON_PREDATES_REPLICATION.into())
}

/// Decide a raw `get` from the HTTP status and body text. `200` carries a
/// [`RawRecordBody`] (absence is `{"record": null}`); `404` →
/// [`upgrade_required`]; any other status surfaces the daemon's body (#195).
fn classify_raw_get_response(status: reqwest::StatusCode, body: &str) -> Result<Option<Record>> {
    match status {
        reqwest::StatusCode::OK => Ok(serde_json::from_str::<RawRecordBody>(body)
            .map_err(se)?
            .record),
        reqwest::StatusCode::NOT_FOUND => Err(upgrade_required()),
        other => Err(read_response_error(other, body)),
    }
}

/// Decide a raw `list`: `200` → keys, `404` → [`upgrade_required`], otherwise
/// the daemon's body.
fn classify_raw_list_response(status: reqwest::StatusCode, body: &str) -> Result<Vec<RecordKey>> {
    match status {
        reqwest::StatusCode::OK => serde_json::from_str(body).map_err(se),
        reqwest::StatusCode::NOT_FOUND => Err(upgrade_required()),
        other => Err(read_response_error(other, body)),
    }
}

/// Decide a `purge`: `200`/`409` carry a [`DeleteOutcome`], `404` →
/// [`upgrade_required`], and any other status (`403` non-admin, `400` bad body)
/// surfaces verbatim, as `delete` does.
fn classify_purge_response(status: reqwest::StatusCode, body: &str) -> Result<DeleteResult> {
    match status {
        reqwest::StatusCode::NOT_FOUND => Err(upgrade_required()),
        other => classify_delete_response(other, body),
    }
}

/// Map a gRPC failure on a replication RPC: `Unimplemented` means the daemon
/// has no such RPC → [`upgrade_required`]; anything else maps as usual.
fn replication_status(s: tonic::Status) -> CoreError {
    if s.code() == tonic::Code::Unimplemented {
        upgrade_required()
    } else {
        status(s)
    }
}

/// Map a gRPC failure on `Put`: `FailedPrecondition` is the daemon's
/// `CoreError::NotFound`, restored as `NotFound(key)`.
fn put_status(s: tonic::Status, key: &RecordKey) -> CoreError {
    if s.code() == tonic::Code::FailedPrecondition {
        CoreError::NotFound(key.clone())
    } else if s.code() == tonic::Code::InvalidArgument {
        // The daemon rejected the call itself; mirrors HTTP's 400 so both
        // transports surface `Invalid` (gonzalo#299).
        CoreError::Invalid(s.message().to_string())
    } else {
        status(s)
    }
}

/// Map a gRPC failure on `PutRaw`: `Unimplemented` → [`upgrade_required`],
/// otherwise as [`put_status`].
fn raw_put_status(s: tonic::Status, key: &RecordKey) -> CoreError {
    if s.code() == tonic::Code::Unimplemented {
        upgrade_required()
    } else {
        put_status(s, key)
    }
}

/// Decode a `ListResponse`'s JSON keys (shared by `List` and `ListRaw`).
fn decode_keys(resp: ListResponse) -> Result<Vec<RecordKey>> {
    resp.keys_json
        .iter()
        .map(|b| serde_json::from_slice::<RecordKey>(b).map_err(se))
        .collect()
}

/// Decide a blob `put`'s result from the HTTP response status and body text.
/// `200 OK` → the (already-known) `hash`; every other status carries a plain
/// text body surfaced verbatim as `Backend("daemon returned <status>: <body>")`
/// — notably `413` (too large), `403` (authz), and `400` (hash mismatch) — so
/// the real failure is never masked (#147).
fn classify_blob_put_response(
    status: reqwest::StatusCode,
    body: &str,
    hash: ContentHash,
) -> Result<ContentHash> {
    match status {
        reqwest::StatusCode::OK => Ok(hash),
        other => Err(CoreError::Backend(format!(
            "daemon returned {other}: {body}"
        ))),
    }
}

/// The error a non-success **read** response carries.
///
/// The write paths surface a non-success status together with the daemon's
/// plain-text body (#147), so a `403` authz denial or a `413` says why. The
/// reads used reqwest's `error_for_status()`, which **discards the body** —
/// leaving a generic "HTTP status client error … for url …" and making a failed
/// remote read strictly harder to debug than a failed write. Same format both
/// ways now (#195).
fn read_response_error(status: reqwest::StatusCode, body: &str) -> CoreError {
    CoreError::Backend(format!("daemon returned {status}: {body}"))
}

/// Pass `resp` through when the daemon answered successfully; otherwise consume
/// its body and fail with [`read_response_error`].
///
/// `404` never reaches here for `get`/`get_blob` — absence is `Ok(None)` and
/// those call sites check it first. For `list`/`list_blobs` a `404` is a real
/// error, since the collection endpoint always exists.
async fn ensure_read_ok(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        // Hand it back untouched: only the caller knows how to decode it.
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(read_response_error(status, &body))
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
    use gonzalo_core::{Body, ContentHash, Identity, Meta, Record, RecordKind};
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
            ancestors: Vec::new(),
            deleted_at: None,
        }
    }

    // ── read paths report failures like the write paths do (#195) ───────────

    /// A `403` on a read names the daemon's reason, not reqwest's generic
    /// "HTTP status client error". This is the whole point of #195.
    #[test]
    fn a_read_failure_carries_the_daemon_body() {
        let err = read_response_error(StatusCode::FORBIDDEN, "namespace not authorized");
        let CoreError::Backend(msg) = err else {
            panic!("read failures are Backend errors");
        };
        assert!(msg.contains("403"), "{msg}");
        assert!(msg.contains("namespace not authorized"), "{msg}");
    }

    /// The harmonization claim itself: for the same status and body, a read
    /// error reads exactly like the write error it used to differ from. If
    /// someone changes one format, this fails rather than letting them drift.
    #[test]
    fn reads_and_writes_report_a_failure_identically() {
        let (status, body) = (StatusCode::FORBIDDEN, "namespace not authorized");
        let read = read_response_error(status, body);
        let write = classify_put_response(status, body).unwrap_err();
        let blob_write =
            classify_blob_put_response(status, body, ContentHash("h".into())).unwrap_err();
        assert_eq!(read.to_string(), write.to_string());
        assert_eq!(read.to_string(), blob_write.to_string());
    }

    /// A `400` is the daemon saying the call itself is wrong (a consumer put of
    /// a tombstone). It comes back as `Invalid`, so a daemon-backed store fails
    /// the way a local one does instead of looking like an outage (#299).
    #[test]
    fn a_bad_request_becomes_invalid_not_backend() {
        let err = classify_put_response(
            StatusCode::BAD_REQUEST,
            "consumer put cannot write a tombstone; use delete_as",
        )
        .unwrap_err();
        assert!(
            matches!(err, CoreError::Invalid(ref m) if m.contains("delete_as")),
            "got {err:?}"
        );
    }

    /// `413` is the other status #147 cared about; it must survive a read too.
    #[test]
    fn a_read_failure_preserves_any_status() {
        let msg =
            read_response_error(StatusCode::PAYLOAD_TOO_LARGE, "blob exceeds 64 MiB").to_string();
        assert!(msg.contains("413"), "{msg}");
        assert!(msg.contains("blob exceeds 64 MiB"), "{msg}");
    }

    /// An empty body still produces a usable message — the status alone is the
    /// signal, and it must not read as a truncated or malformed error.
    #[test]
    fn a_read_failure_with_no_body_still_names_the_status() {
        let msg = read_response_error(StatusCode::BAD_GATEWAY, "").to_string();
        assert!(msg.contains("502"), "{msg}");
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
    /// error — as `Invalid`, since a `400` is always the call being wrong
    /// rather than the store failing (#299).
    #[test]
    fn bad_request_surfaces_status_and_body() {
        let body = "path/body key disagreement";
        let err = classify_put_response(StatusCode::BAD_REQUEST, body).unwrap_err();
        match err {
            CoreError::Invalid(msg) => {
                assert!(msg.contains("400"), "want status 400 in {msg:?}");
                assert!(msg.contains(body), "want daemon body in {msg:?}");
            }
            other => panic!("expected Invalid error, got {other:?}"),
        }
    }

    #[test]
    fn blob_put_ok_returns_the_hash() {
        let content = b"blob body";
        let hash = ContentHash::of(content);
        let result = classify_blob_put_response(StatusCode::OK, "", hash.clone()).unwrap();
        assert_eq!(result, hash);
    }

    #[test]
    fn blob_put_413_surfaces_status_and_body() {
        let hash = ContentHash::of(b"x");
        let err = classify_blob_put_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "blob exceeds max size",
            hash,
        )
        .unwrap_err();
        match err {
            CoreError::Backend(msg) => {
                assert!(msg.contains("413"), "want status 413 in {msg:?}");
                assert!(
                    msg.contains("blob exceeds max size"),
                    "want body in {msg:?}"
                );
            }
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    #[test]
    fn blob_put_400_mismatch_surfaces_status_and_body() {
        let hash = ContentHash::of(b"x");
        let err = classify_blob_put_response(
            StatusCode::BAD_REQUEST,
            "blob content does not match the URL hash",
            hash,
        )
        .unwrap_err();
        match err {
            CoreError::Backend(msg) => {
                assert!(msg.contains("400"), "want status 400 in {msg:?}");
                assert!(msg.contains("does not match"), "want body in {msg:?}");
            }
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    // ── replication surface: upgrade error, never a fallback (#203) ─────────

    fn upgrade_error() -> String {
        CoreError::Backend(DAEMON_PREDATES_REPLICATION.into()).to_string()
    }

    #[test]
    fn raw_get_200_null_is_absent_and_200_record_is_present() {
        assert_eq!(
            classify_raw_get_response(StatusCode::OK, r#"{"record":null}"#).unwrap(),
            None
        );
        let rec = sample_record();
        let body = serde_json::to_string(&RawRecordBody {
            record: Some(rec.clone()),
        })
        .unwrap();
        assert_eq!(
            classify_raw_get_response(StatusCode::OK, &body).unwrap(),
            Some(rec)
        );
    }

    #[test]
    fn replication_404_is_the_upgrade_error() {
        let key = RecordKey::new("ns", "col", "id");
        for err in [
            classify_raw_get_response(StatusCode::NOT_FOUND, "")
                .unwrap_err()
                .to_string(),
            classify_raw_list_response(StatusCode::NOT_FOUND, "")
                .unwrap_err()
                .to_string(),
            classify_raw_put_response(&key, StatusCode::NOT_FOUND, "")
                .unwrap_err()
                .to_string(),
            classify_purge_response(StatusCode::NOT_FOUND, "")
                .unwrap_err()
                .to_string(),
        ] {
            assert_eq!(err, upgrade_error());
        }
    }

    #[test]
    fn put_412_is_not_found_for_the_record_key() {
        let key = RecordKey::new("ns", "col", "id");
        assert!(matches!(
            classify_put_response_for(&key, StatusCode::PRECONDITION_FAILED, "record not found"),
            Err(CoreError::NotFound(k)) if k == key
        ));
        assert!(matches!(
            classify_raw_put_response(&key, StatusCode::PRECONDITION_FAILED, "record not found"),
            Err(CoreError::NotFound(k)) if k == key
        ));
        // The consumer put route never turns a 404 into the upgrade error.
        let msg = classify_put_response_for(&key, StatusCode::NOT_FOUND, "nope")
            .unwrap_err()
            .to_string();
        assert_ne!(msg, upgrade_error());
    }

    #[test]
    fn replication_403_keeps_the_daemon_body() {
        let body = "principal \"w\" is not an admin; purge requires admin";
        let msg = classify_purge_response(StatusCode::FORBIDDEN, body)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("403") && msg.contains(body), "{msg}");
        let msg = classify_raw_list_response(StatusCode::FORBIDDEN, "nope")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("403") && msg.contains("nope"), "{msg}");
    }

    #[test]
    fn purge_200_and_409_parse_delete_outcomes() {
        let ok = serde_json::to_string(&DeleteOutcome::Deleted).unwrap();
        assert_eq!(
            classify_purge_response(StatusCode::OK, &ok).unwrap(),
            DeleteResult::Deleted
        );
        let rec = sample_record();
        let conflict = serde_json::to_string(&DeleteOutcome::Conflict {
            conflict: Box::new(Conflict {
                key: rec.key.clone(),
                expected: None,
                current: rec,
            }),
        })
        .unwrap();
        assert!(matches!(
            classify_purge_response(StatusCode::CONFLICT, &conflict).unwrap(),
            DeleteResult::Conflict(_)
        ));
    }

    #[test]
    fn grpc_status_mapping_for_replication_and_puts() {
        let key = RecordKey::new("ns", "col", "id");
        assert_eq!(
            replication_status(tonic::Status::unimplemented("GetRaw")).to_string(),
            upgrade_error()
        );
        assert_ne!(
            replication_status(tonic::Status::permission_denied("no")).to_string(),
            upgrade_error()
        );
        assert!(matches!(
            put_status(tonic::Status::failed_precondition("record not found"), &key),
            CoreError::NotFound(k) if k == key
        ));
        assert_eq!(
            raw_put_status(tonic::Status::unimplemented("PutRaw"), &key).to_string(),
            upgrade_error()
        );
        assert!(matches!(
            raw_put_status(tonic::Status::failed_precondition("x"), &key),
            CoreError::NotFound(_)
        ));
    }

    /// Spec §6.5: against a daemon without the replication routes, every
    /// replication call fails with the upgrade error and **no consumer route is
    /// ever hit**.
    #[tokio::test]
    async fn http_old_daemon_errors_and_never_falls_back_to_consumer_routes() {
        use wiremock::matchers::path_regex;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Any consumer-route hit is a fallback: verification fails the test.
        Mock::given(path_regex(r"^/v1/(records|keys)(/|$)"))
            .respond_with(ResponseTemplate::new(200))
            .named("consumer route (fallback)")
            .expect(0)
            .mount(&server)
            .await;
        // An old daemon has no replication routes.
        Mock::given(path_regex(r"^/v1/(raw|purge)/"))
            .respond_with(ResponseTemplate::new(404))
            .named("replication route")
            .expect(4)
            .mount(&server)
            .await;

        let store = ServerStore::http(&server.uri()).unwrap();
        let key = RecordKey::new("ns", "col", "id");
        let errors = [
            store.get_raw(&key).await.unwrap_err(),
            store.list_raw(&KeyPrefix::default()).await.unwrap_err(),
            store.put_raw(sample_record(), None).await.unwrap_err(),
            store
                .purge(&key, Revision::initial(b"x"))
                .await
                .unwrap_err(),
        ];
        for err in errors {
            assert_eq!(err.to_string(), upgrade_error());
        }
        server.verify().await;
    }

    /// A tonic server with no services answers every RPC `Unimplemented`,
    /// which is what a pre-#203 daemon does for the replication RPCs.
    #[tokio::test]
    async fn grpc_old_daemon_errors_with_the_upgrade_message() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_routes(tonic::service::Routes::default())
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );
        let store = ServerStore::grpc(format!("http://{addr}")).await.unwrap();
        let key = RecordKey::new("ns", "col", "id");
        let errors = [
            store.get_raw(&key).await.unwrap_err(),
            store.list_raw(&KeyPrefix::default()).await.unwrap_err(),
            store.put_raw(sample_record(), None).await.unwrap_err(),
            store
                .purge(&key, Revision::initial(b"x"))
                .await
                .unwrap_err(),
        ];
        for err in errors {
            assert_eq!(err.to_string(), upgrade_error());
        }
    }

    // ── delete_as sends its author (R1, spec §3.6 amended in 4355f4d) ───────

    /// A struct-based matcher (wiremock's `Match` trait has no built-in
    /// negation) asserting the request body carries no `author` key at all —
    /// `DeleteBody`'s `skip_serializing_if` omits it entirely when `None`.
    struct NoAuthorField;
    impl wiremock::Match for NoAuthorField {
        fn matches(&self, request: &wiremock::Request) -> bool {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .map(|v| v.get("author").is_none())
                .unwrap_or(false)
        }
    }

    #[tokio::test]
    async fn http_delete_as_sends_the_author() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Identity's real serde shape is `{"id": ..., "display": ...}`, not a
        // bare string, so match on the nested field (wiremock's partial-JSON
        // matcher is inclusive at every level).
        Mock::given(method("DELETE"))
            .and(path("/v1/records/ns/col/with-author"))
            .and(body_partial_json(
                serde_json::json!({"author": {"id": "origin"}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(&DeleteOutcome::Deleted))
            .named("delete with author")
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("DELETE"))
            .and(path("/v1/records/ns/col/no-author"))
            .and(NoAuthorField)
            .respond_with(ResponseTemplate::new(200).set_body_json(&DeleteOutcome::Deleted))
            .named("delete without author")
            .expect(1)
            .mount(&server)
            .await;

        let store = ServerStore::http(&server.uri()).unwrap();

        let with_key = RecordKey::new("ns", "col", "with-author");
        let result = store
            .delete_as(&with_key, None, Some(Identity::new("origin")))
            .await
            .unwrap();
        assert_eq!(result, DeleteResult::Deleted);

        let no_key = RecordKey::new("ns", "col", "no-author");
        let result = store.delete_as(&no_key, None, None).await.unwrap();
        assert_eq!(result, DeleteResult::Deleted);

        server.verify().await;
    }
}
