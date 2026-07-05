//! S3-compatible object-store substrate. One JSON object per record at
//! key `namespace/collection/id.json`.

use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::ProvideErrorMetadata;
use gonzalo_core::{
    CoreError, KeyPrefix, PutResult, Record, RecordKey, Result, Revision, object_key,
    store::Conflict,
};

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
    /// `endpoint` is `Some`, target an S3-compatible server (MinIO, etc.)
    /// with path-style addressing.
    pub async fn connect(bucket: impl Into<String>, endpoint: Option<String>) -> Self {
        let base = aws_config::load_from_env().await;
        let mut builder = aws_sdk_s3::config::Builder::from(&base);
        if let Some(ep) = endpoint {
            builder = builder.endpoint_url(ep).force_path_style(true);
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
            if resp.is_truncated().unwrap_or(false) {
                continuation = resp.next_continuation_token().map(str::to_string);
            } else {
                break;
            }
        }
        Ok(out)
    }
}

/// Parse `namespace/collection/id.json` back into a `RecordKey`. Returns
/// `None` for objects that don't match the expected three-part `.json` shape.
fn parse_object_key(s: &str) -> Option<RecordKey> {
    let rest = s.strip_suffix(".json")?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() == 3 {
        Some(RecordKey::new(parts[0], parts[1], parts[2]))
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
    fn precondition_failed_is_classified_by_code() {
        assert!(is_precondition_failed(Some("PreconditionFailed")));
        assert!(!is_precondition_failed(Some("AccessDenied")));
        assert!(!is_precondition_failed(None));
    }
}
