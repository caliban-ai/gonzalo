//! S3-compatible object-store substrate. One JSON object per record at
//! key `namespace/collection/id.json`.

use async_trait::async_trait;
use aws_sdk_s3::Client;
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
        Self { client, bucket: bucket.into() }
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
        let obj = object_key(key);
        match self.client.get_object().bucket(&self.bucket).key(&obj).send().await {
            Ok(resp) => {
                let data = resp
                    .body
                    .collect()
                    .await
                    .map_err(|e| CoreError::Backend(e.to_string()))?
                    .into_bytes();
                Ok(Some(
                    serde_json::from_slice(&data).map_err(|e| CoreError::Serde(e.to_string()))?,
                ))
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

#[async_trait]
impl gonzalo_core::Store for S3Store {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.read(key).await
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // NOTE(TOCTOU): read-then-write without conditional PUT; acceptable for
        // M2. Native If-Match/If-None-Match conditional writes deferred.
        let current = self.read(&record.key).await?;
        let current_rev = current.as_ref().map(|r| r.revision.clone());
        if current_rev != expected {
            if let Some(cur) = current {
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
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(object_key(&record.key))
            .body(bytes.into())
            .send()
            .await
            .map_err(|e| CoreError::Backend(e.into_service_error().to_string()))?;
        Ok(PutResult::Committed(record.revision))
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
}
