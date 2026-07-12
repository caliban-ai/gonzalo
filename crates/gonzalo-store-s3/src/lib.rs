//! S3-compatible object-store substrate. One JSON object per record at
//! key `namespace/collection/id.json`.

use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::ProvideErrorMetadata;
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DeleteResult, KeyPrefix, PutResult, Record, RecordKey,
    Result, Revision, decode_segment, object_key, store::Conflict,
};

/// Key prefix under which content-addressed blobs live (`blobs/<hash>`), kept
/// separate from record objects (`namespace/collection/id.json`).
const BLOB_PREFIX: &str = "blobs/";

pub struct S3Store {
    client: Client,
    bucket: String,
}

impl S3Store {
    /// Build a store from an explicit client and bucket. Use
    /// [`S3Store::connect`] for the common env/endpoint path.
    pub fn new(client: Client, bucket: impl Into<String>) -> Self {
        Self {
            client,
            bucket: bucket.into(),
        }
    }

    /// Connect using the ambient AWS config (env, profile, IRSA, etc.). If
    /// `endpoint` is `Some`, target an S3-compatible server (MinIO/Garage, etc.)
    /// with path-style addressing; if `region` is `Some`, override the ambient
    /// region (else the AWS env/profile region applies).
    pub async fn connect(
        bucket: impl Into<String>,
        endpoint: Option<String>,
        region: Option<String>,
    ) -> Self {
        let base = aws_config::load_from_env().await;
        let mut builder = aws_sdk_s3::config::Builder::from(&base);
        if let Some(ep) = endpoint {
            builder = builder.endpoint_url(ep).force_path_style(true);
        }
        if let Some(r) = region {
            builder = builder.region(aws_sdk_s3::config::Region::new(r));
        }
        let client = Client::from_conf(builder.build());
        Self::new(client, bucket)
    }

    async fn read(&self, key: &RecordKey) -> Result<Option<Record>> {
        Ok(self.read_with_etag(key).await?.map(|(rec, _)| rec))
    }

    /// Like [`read`](Self::read) but also returns the object's S3 ETag, which
    /// [`put`](gonzalo_core::Store::put) feeds back as an `If-Match` precondition
    /// to make the compare-and-swap atomic (closing the read-then-write TOCTOU).
    async fn read_with_etag(&self, key: &RecordKey) -> Result<Option<(Record, String)>> {
        let obj = object_key(key);
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&obj)
            .send()
            .await
        {
            Ok(resp) => {
                let etag = resp.e_tag().unwrap_or_default().to_string();
                let data = resp
                    .body
                    .collect()
                    .await
                    .map_err(|e| CoreError::Backend(e.to_string()))?
                    .into_bytes();
                let record =
                    serde_json::from_slice(&data).map_err(|e| CoreError::Serde(e.to_string()))?;
                Ok(Some((record, etag)))
            }
            Err(e) => {
                let svc = e.into_service_error();
                if svc.is_no_such_key() {
                    Ok(None)
                } else {
                    Err(CoreError::Backend(svc.to_string()))
                }
            }
        }
    }
}

/// The S3 precondition that enforces OCC atomically at write time, chosen from
/// the caller's `expected` revision and the ETag read for the object.
#[derive(Debug, PartialEq, Eq)]
enum Precondition {
    /// Create only if the object is still absent (`If-None-Match: *`).
    IfAbsent,
    /// Replace only if the object still carries this ETag (`If-Match: <etag>`).
    IfMatch(String),
}

/// Map `(expected, etag)` to the write precondition. A create (`expected =
/// None`) requires the object to still be absent; an update (`expected =
/// Some`, so the object was read with an `etag`) requires that exact ETag. The
/// business-level OCC check runs first, so the `Some`-without-etag case can't
/// reach here; `IfAbsent` is a safe total default for it.
fn precondition(expected: &Option<Revision>, etag: Option<&str>) -> Precondition {
    match (expected, etag) {
        (Some(_), Some(tag)) => Precondition::IfMatch(tag.to_string()),
        _ => Precondition::IfAbsent,
    }
}

/// Whether an S3 error code denotes a failed write precondition (HTTP 412) —
/// i.e. a concurrent writer won the race, which OCC surfaces as a `Conflict`.
fn is_precondition_failed(code: Option<&str>) -> bool {
    matches!(code, Some("PreconditionFailed"))
}

/// Decide the continuation token for the next `list_objects_v2` page, driving
/// pagination off token *presence* rather than the `is_truncated` flag. A
/// well-behaved backend only returns a token when there is more to fetch, but a
/// misbehaving one can report `is_truncated = true` yet omit the token; keying
/// off the flag would then re-request page 1 forever. So: if a token is present
/// we continue with it, otherwise we terminate — regardless of `is_truncated`.
/// This guarantees the pagination loop always makes progress or stops.
fn next_continuation(_is_truncated: Option<bool>, token: Option<&str>) -> Option<String> {
    token.map(str::to_string)
}

#[async_trait]
impl gonzalo_core::Store for S3Store {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.read(key).await
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // Read the current object *and its ETag*, then make the write itself
        // conditional on that ETag (`If-Match`) or on absence (`If-None-Match:
        // *`). The business OCC check below is a fast pre-check; the S3
        // precondition is what makes the compare-and-swap atomic, so a writer
        // that slips in between our read and write loses the race with a 412
        // rather than silently clobbering — closing the read-then-write TOCTOU.
        let current = self.read_with_etag(&record.key).await?;
        let current_rev = current.as_ref().map(|(r, _)| r.revision.clone());
        if current_rev != expected {
            if let Some((cur, _)) = current {
                return Ok(PutResult::Conflict(Box::new(Conflict {
                    key: record.key.clone(),
                    expected,
                    current: cur,
                })));
            }
            return Err(CoreError::NotFound(record.key.clone()));
        }

        let bytes =
            serde_json::to_vec_pretty(&record).map_err(|e| CoreError::Serde(e.to_string()))?;
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(object_key(&record.key))
            .body(bytes.into());
        req = match precondition(&expected, current.as_ref().map(|(_, tag)| tag.as_str())) {
            Precondition::IfAbsent => req.if_none_match("*"),
            Precondition::IfMatch(tag) => req.if_match(tag),
        };

        match req.send().await {
            Ok(_) => Ok(PutResult::Committed(record.revision)),
            Err(e) => {
                let svc = e.into_service_error();
                // A 412 means a concurrent writer changed the object between our
                // read and conditional write: re-read for the fresh state and
                // surface the normal, recoverable Conflict (NotFound if it was
                // concurrently deleted).
                if is_precondition_failed(svc.code()) {
                    return match self.read(&record.key).await? {
                        Some(cur) => Ok(PutResult::Conflict(Box::new(Conflict {
                            key: record.key.clone(),
                            expected,
                            current: cur,
                        }))),
                        None => Err(CoreError::NotFound(record.key.clone())),
                    };
                }
                Err(CoreError::Backend(svc.to_string()))
            }
        }
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        let mut s3_prefix = String::new();
        if let Some(ns) = &prefix.namespace {
            s3_prefix.push_str(&gonzalo_core::segment(ns));
            s3_prefix.push('/');
            if let Some(col) = &prefix.collection {
                s3_prefix.push_str(&gonzalo_core::segment(col));
                s3_prefix.push('/');
            }
        }
        let mut out = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut req = self.client.list_objects_v2().bucket(&self.bucket);
            if !s3_prefix.is_empty() {
                req = req.prefix(&s3_prefix);
            }
            if let Some(token) = &continuation {
                req = req.continuation_token(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| CoreError::Backend(e.into_service_error().to_string()))?;
            for obj in resp.contents() {
                if let Some(k) = obj.key()
                    && let Some(key) = parse_object_key(k)
                    && prefix.matches(&key)
                {
                    out.push(key);
                }
            }
            match next_continuation(resp.is_truncated(), resp.next_continuation_token()) {
                Some(token) => continuation = Some(token),
                None => break,
            }
        }
        Ok(out)
    }

    async fn delete(&self, key: &RecordKey, expected: Option<Revision>) -> Result<DeleteResult> {
        // Unconditional delete (`expected = None`): S3 delete of an absent key
        // already succeeds, so this is an idempotent `Deleted`.
        let Some(want) = expected.clone() else {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(object_key(key))
                .send()
                .await
                .map_err(|e| CoreError::Backend(e.into_service_error().to_string()))?;
            return Ok(DeleteResult::Deleted);
        };

        // Conditional delete: read the current object *and its ETag*, then make
        // the removal conditional on that ETag (`If-Match`) so a writer that
        // slips in between our read and delete loses the race with a 412 — the
        // same TOCTOU close as `put`. An already-absent key is an idempotent
        // `Deleted` (the revision is already gone — nothing to conflict on).
        let Some((current, etag)) = self.read_with_etag(key).await? else {
            return Ok(DeleteResult::Deleted);
        };
        if current.revision != want {
            return Ok(DeleteResult::Conflict(Box::new(Conflict {
                key: key.clone(),
                expected,
                current,
            })));
        }

        match self
            .client
            .delete_object()
            .bucket(&self.bucket)
            .key(object_key(key))
            .if_match(etag)
            .send()
            .await
        {
            Ok(_) => Ok(DeleteResult::Deleted),
            Err(e) => {
                let svc = e.into_service_error();
                // A 412 means a concurrent writer changed the object between our
                // read and conditional delete: re-read for the fresh state and
                // surface the normal, recoverable Conflict (or `Deleted` if it
                // was concurrently removed — the revision is already gone).
                if is_precondition_failed(svc.code()) {
                    return match self.read(key).await? {
                        Some(cur) => Ok(DeleteResult::Conflict(Box::new(Conflict {
                            key: key.clone(),
                            expected,
                            current: cur,
                        }))),
                        None => Ok(DeleteResult::Deleted),
                    };
                }
                Err(CoreError::Backend(svc.to_string()))
            }
        }
    }
}

#[async_trait]
impl BlobStore for S3Store {
    async fn put_blob(&self, content: &[u8]) -> Result<ContentHash> {
        let hash = ContentHash::of(content);
        let key = format!("{BLOB_PREFIX}{}", hash.0);
        // Content-addressed + write-if-absent: an existing blob at this key is
        // byte-identical, so `If-None-Match: *` turns a re-upload into a no-op
        // (a 412 just means it's already stored). Idempotent and bandwidth-cheap.
        match self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .if_none_match("*")
            .body(content.to_vec().into())
            .send()
            .await
        {
            Ok(_) => Ok(hash),
            Err(e) => {
                let svc = e.into_service_error();
                if is_precondition_failed(svc.code()) {
                    Ok(hash) // already present — no-op
                } else {
                    Err(CoreError::Backend(svc.to_string()))
                }
            }
        }
    }

    async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        let key = format!("{BLOB_PREFIX}{}", hash.0);
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(resp) => {
                let data = resp
                    .body
                    .collect()
                    .await
                    .map_err(|e| CoreError::Backend(e.to_string()))?
                    .into_bytes();
                Ok(Some(data.to_vec()))
            }
            Err(e) => {
                let svc = e.into_service_error();
                if svc.is_no_such_key() {
                    Ok(None)
                } else {
                    Err(CoreError::Backend(svc.to_string()))
                }
            }
        }
    }

    async fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        let mut out = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(BLOB_PREFIX);
            if let Some(token) = &continuation {
                req = req.continuation_token(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| CoreError::Backend(e.into_service_error().to_string()))?;
            for obj in resp.contents() {
                if let Some(k) = obj.key()
                    && let Some(hash) = blob_hash_from_key(k)
                {
                    out.push(hash);
                }
            }
            match next_continuation(resp.is_truncated(), resp.next_continuation_token()) {
                Some(token) => continuation = Some(token),
                None => break,
            }
        }
        Ok(out)
    }

    async fn delete_blob(&self, hash: &ContentHash) -> Result<()> {
        let key = format!("{BLOB_PREFIX}{}", hash.0);
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| CoreError::Backend(e.into_service_error().to_string()))?;
        Ok(()) // S3 delete of an absent key succeeds — idempotent
    }
}

/// Parse a blob object key `blobs/<hash>` back into a [`ContentHash`]. Returns
/// `None` for anything that isn't exactly one segment under `blobs/` — so a
/// record object that happens to live in a `blobs` namespace
/// (`blobs/<col>/<id>.json`, which still has a `/`) is never mistaken for a blob.
fn blob_hash_from_key(key: &str) -> Option<ContentHash> {
    let rest = key.strip_prefix(BLOB_PREFIX)?;
    if rest.is_empty() || rest.contains('/') || rest.contains('.') {
        return None;
    }
    Some(ContentHash(rest.to_string()))
}

/// Parse `namespace/collection/id.json` back into a `RecordKey`, decoding each
/// component (the exact inverse of `object_key`). Returns `None` for objects
/// that don't match the expected three-part `.json` shape. Since every literal
/// `/` in a component is escaped, splitting on `/` always yields exactly the
/// three separators' worth of parts.
fn parse_object_key(s: &str) -> Option<RecordKey> {
    let rest = s.strip_suffix(".json")?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() == 3 {
        Some(RecordKey::new(
            decode_segment(parts[0]),
            decode_segment(parts[1]),
            decode_segment(parts[2]),
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrips_object_key() {
        let k = RecordKey::new("ns", "col", "id");
        assert_eq!(parse_object_key(&object_key(&k)), Some(k));
    }

    #[test]
    fn parse_roundtrips_special_char_keys() {
        // Keys with `.`, `/`, spaces, and `%` must survive the object-key
        // round-trip and stay distinct (no collision onto one object).
        for k in [
            RecordKey::new("a/b", "c.d", "e/f"),
            RecordKey::new("ns", "col", "v1.0"),
            RecordKey::new("ns", "col", "v1_0"),
            RecordKey::new("50% off", "café", "🚀"),
        ] {
            assert_eq!(parse_object_key(&object_key(&k)), Some(k));
        }
        assert_ne!(
            object_key(&RecordKey::new("ns", "col", "v1.0")),
            object_key(&RecordKey::new("ns", "col", "v1_0")),
        );
    }

    #[test]
    fn parse_rejects_non_json_or_wrong_depth() {
        assert_eq!(parse_object_key("a/b/c.txt"), None);
        assert_eq!(parse_object_key("a/b.json"), None);
        assert_eq!(parse_object_key("a/b/c/d.json"), None);
    }

    fn rev() -> Revision {
        Revision::initial(b"x")
    }

    #[test]
    fn create_uses_if_absent() {
        // expected = None → create-only, regardless of any etag.
        assert_eq!(precondition(&None, None), Precondition::IfAbsent);
        assert_eq!(
            precondition(&None, Some("\"etag\"")),
            Precondition::IfAbsent
        );
    }

    #[test]
    fn update_uses_if_match_on_the_read_etag() {
        assert_eq!(
            precondition(&Some(rev()), Some("\"abc123\"")),
            Precondition::IfMatch("\"abc123\"".to_string())
        );
    }

    #[test]
    fn next_continuation_terminates_when_token_absent() {
        // The pagination bug: a backend reports more pages but omits the token.
        // Keying off `is_truncated` would loop forever; we must terminate.
        assert_eq!(next_continuation(Some(true), None), None);
        // No token, not truncated → also terminate (the normal last page).
        assert_eq!(next_continuation(Some(false), None), None);
        assert_eq!(next_continuation(None, None), None);
    }

    #[test]
    fn next_continuation_advances_when_token_present() {
        // A token means fetch the next page, regardless of the flag's value.
        assert_eq!(
            next_continuation(Some(true), Some("t1")),
            Some("t1".to_string())
        );
        assert_eq!(
            next_continuation(Some(false), Some("t2")),
            Some("t2".to_string())
        );
        assert_eq!(next_continuation(None, Some("t3")), Some("t3".to_string()));
    }

    #[test]
    fn precondition_failed_is_classified_by_code() {
        assert!(is_precondition_failed(Some("PreconditionFailed")));
        assert!(!is_precondition_failed(Some("AccessDenied")));
        assert!(!is_precondition_failed(None));
    }

    #[test]
    fn blob_key_roundtrips_and_rejects_records() {
        let h = ContentHash::of(b"slice bytes");
        let key = format!("{BLOB_PREFIX}{}", h.0);
        assert_eq!(blob_hash_from_key(&key), Some(h));
        // A record object under a `blobs` namespace has a nested path + `.json`
        // and must never be read back as a blob hash.
        assert_eq!(blob_hash_from_key("blobs/col/id.json"), None);
        assert_eq!(blob_hash_from_key("ns/col/id.json"), None);
        assert_eq!(blob_hash_from_key("blobs/"), None);
    }
}
