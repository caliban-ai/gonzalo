//! S3-compatible object-store substrate. One JSON object per record at
//! key `namespace/collection/id.json`.

use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::ProvideErrorMetadata;
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DEFAULT_ANCESTOR_CAP, DeletePlan, DeleteResult, Identity,
    KeyPrefix, PurgePlan, PutPlan, PutResult, Record, RecordKey, Result, Revision, decode_segment,
    now_ms, object_key, plan_delete, plan_purge, plan_put, plan_put_raw, validate_ancestor_cap,
};

/// Key prefix under which content-addressed blobs live (`blobs/<hash>`), kept
/// separate from record objects (`namespace/collection/id.json`).
const BLOB_PREFIX: &str = "blobs/";

pub struct S3Store {
    client: Client,
    bucket: String,
    /// Maximum `Record::ancestors` length kept on every committed write
    /// (spec §3.9). Defaults to [`DEFAULT_ANCESTOR_CAP`].
    cap: usize,
}

impl S3Store {
    /// Build a store from an explicit client and bucket. Use
    /// [`S3Store::connect`] for the common env/endpoint path.
    pub fn new(client: Client, bucket: impl Into<String>) -> Self {
        Self {
            client,
            bucket: bucket.into(),
            cap: DEFAULT_ANCESTOR_CAP,
        }
    }

    /// Override the ancestor cap (spec §3.9). A cap of `0` is rejected.
    pub fn with_ancestor_cap(mut self, cap: usize) -> Result<Self> {
        self.cap = validate_ancestor_cap(cap)?;
        Ok(self)
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
    /// [`write_planned`](Self::write_planned) feeds back as the write
    /// precondition to make every compare-and-swap atomic (closing the
    /// read-then-write TOCTOU). Returns tombstones: this is a raw read.
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

    /// Every record key under `prefix`, tombstones included (the raw listing).
    /// Paginates `ListObjectsV2` off the continuation token (see
    /// [`next_continuation`]).
    async fn list_keys(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
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

    /// Serialize `record` and `PutObject` it at its key under `pre`. A writer
    /// that changed the object after our read makes this a 412, reported as
    /// [`WriteOutcome::PreconditionFailed`] instead of clobbering its write.
    async fn put_record_if(&self, record: &Record, pre: Precondition) -> Result<WriteOutcome> {
        let bytes =
            serde_json::to_vec_pretty(record).map_err(|e| CoreError::Serde(e.to_string()))?;
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(object_key(&record.key))
            .body(bytes.into());
        req = match pre {
            Precondition::IfAbsent => req.if_none_match("*"),
            Precondition::IfMatch(tag) => req.if_match(tag),
        };
        match req.send().await {
            Ok(_) => Ok(WriteOutcome::Applied),
            Err(e) => {
                let svc = e.into_service_error();
                if is_lost_race(svc.code()) {
                    Ok(WriteOutcome::PreconditionFailed)
                } else {
                    Err(CoreError::Backend(svc.to_string()))
                }
            }
        }
    }

    /// `DeleteObject` at `key` only if it still carries `etag` (`If-Match`).
    async fn delete_record_if_match(&self, key: &RecordKey, etag: String) -> Result<WriteOutcome> {
        match self
            .client
            .delete_object()
            .bucket(&self.bucket)
            .key(object_key(key))
            .if_match(etag)
            .send()
            .await
        {
            Ok(_) => Ok(WriteOutcome::Applied),
            Err(e) => {
                let svc = e.into_service_error();
                if is_lost_race(svc.code()) {
                    Ok(WriteOutcome::PreconditionFailed)
                } else {
                    Err(CoreError::Backend(svc.to_string()))
                }
            }
        }
    }

    /// The compare-and-swap loop behind every mutating method. Each attempt
    /// reads the object and its ETag, asks `plan` what to do given that exact
    /// record, and carries it out with a conditional write gated on the same
    /// ETag. A lost race (412, 409, or the object vanishing to a concurrent
    /// purge) means a concurrent writer changed the object after our read, so
    /// the next attempt re-reads and re-plans. The planner therefore always
    /// decides against the true current record, as it does under the fs and
    /// git stores' locks. Gives up after [`MAX_WRITE_ATTEMPTS`] attempts.
    async fn write_planned<T, P>(&self, key: &RecordKey, plan: P) -> Result<T>
    where
        T: Send,
        P: Fn(Option<&Record>) -> Result<Planned<T>> + Sync,
    {
        let plan = &plan;
        retry_on_lost_race(key, move || async move {
            let current = self.read_with_etag(key).await?;
            let etag = current.as_ref().map(|(_, tag)| tag.as_str());
            let (outcome, answer) = match plan(current.as_ref().map(|(rec, _)| rec))? {
                Planned::Finish(answer) => return Ok(Step::Done(answer)),
                Planned::Put(record, answer) => (
                    self.put_record_if(&record, precondition(etag)).await?,
                    answer,
                ),
                Planned::Remove(answer) => {
                    // Planners only remove a record they were given, so an ETag
                    // was read. The empty fallback can never match; it would
                    // just 412 and re-plan.
                    let tag = etag.unwrap_or_default().to_string();
                    (self.delete_record_if_match(key, tag).await?, answer)
                }
            };
            Ok(match outcome {
                WriteOutcome::Applied => Step::Done(answer),
                WriteOutcome::PreconditionFailed => Step::Retry,
            })
        })
        .await
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

/// Map the ETag read for the object to the write precondition. If an object
/// was read (a live record *or a tombstone*), the write must replace exactly
/// that version (`If-Match`). If none was read, it must still be absent
/// (`If-None-Match: *`). This depends on what's stored, not on the caller's
/// `expected`: recreation is `put(_, None)` over an existing tombstone object,
/// so keying off `expected` would pick `IfAbsent` and fail with 412 every time.
fn precondition(etag: Option<&str>) -> Precondition {
    match etag {
        Some(tag) => Precondition::IfMatch(tag.to_string()),
        None => Precondition::IfAbsent,
    }
}

/// Whether an S3 error code denotes a failed write precondition (HTTP 412) —
/// i.e. a concurrent writer won the race, which OCC surfaces as a `Conflict`.
fn is_precondition_failed(code: Option<&str>) -> bool {
    matches!(code, Some("PreconditionFailed"))
}

/// Whether a conditional record write lost to a change made after our read:
/// a failed precondition (412), a conflicting in-flight conditional write
/// (409), or the object vanishing (a concurrent purge). Each re-reads and
/// re-plans in `write_planned`.
fn is_lost_race(code: Option<&str>) -> bool {
    matches!(
        code,
        Some("PreconditionFailed" | "ConditionalRequestConflict" | "NoSuchKey")
    )
}

/// Consumer view of a raw read: a tombstone reads as absent (spec §3.2).
fn visible(record: Option<Record>) -> Option<Record> {
    record.filter(|r| !r.is_tombstone())
}

/// Whether consumer `list` includes a key, given the raw read of its object.
/// Tombstones and keys purged since the listing (NotFound) are excluded. An
/// object that fails to decode stays listed, so `get` on that key surfaces the
/// error. Any other read error (network, 5xx, permission) fails the whole
/// `list`: on s3 it may be transient, and listing a key that may be a
/// tombstone would be wrong. fs and git differ: they keep every unreadable
/// entry listed.
fn listed_as_live(read: Result<Option<Record>>) -> Result<bool> {
    match read {
        Ok(record) => Ok(visible(record).is_some()),
        Err(CoreError::Serde(_)) => Ok(true),
        Err(e) => Err(e),
    }
}

/// One planner decision, translated into the S3 action that carries it out
/// and the answer to return once that action lands.
// `Record` is the natural payload for a write step; boxing it would ripple
// through every `*_step` signature for no benefit at this call volume.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq, Eq)]
enum Planned<T> {
    /// No write needed; return this answer.
    Finish(T),
    /// `PutObject` this record under [`precondition`], then return the answer.
    Put(Record, T),
    /// `DeleteObject` with `If-Match` on the read ETag, then return the answer.
    Remove(T),
}

/// Translate a [`PutPlan`] (from `plan_put` or `plan_put_raw`).
fn put_step(key: &RecordKey, plan: PutPlan) -> Result<Planned<PutResult>> {
    match plan {
        PutPlan::Write(record) => {
            let committed = PutResult::Committed(record.revision.clone());
            Ok(Planned::Put(record, committed))
        }
        PutPlan::Conflict(conflict) => Ok(Planned::Finish(PutResult::Conflict(conflict))),
        PutPlan::NotFound => Err(CoreError::NotFound(key.clone())),
        // Only consumer `plan_put` produces this, for a `RecordKind::Tombstone`
        // record: deletes go through `delete_as`, replication through `put_raw`.
        PutPlan::Rejected(reason) => Err(CoreError::Invalid(reason.to_string())),
    }
}

/// Translate a [`DeletePlan`]: a tombstone is written with `PutObject`.
fn delete_step(plan: DeletePlan) -> Planned<DeleteResult> {
    match plan {
        DeletePlan::Write(tombstone) => Planned::Put(tombstone, DeleteResult::Deleted),
        DeletePlan::Noop => Planned::Finish(DeleteResult::Deleted),
        DeletePlan::Conflict(conflict) => Planned::Finish(DeleteResult::Conflict(conflict)),
    }
}

/// Translate a [`PurgePlan`]: the only plan that physically removes an object.
fn purge_step(plan: PurgePlan) -> Planned<DeleteResult> {
    match plan {
        PurgePlan::Remove => Planned::Remove(DeleteResult::Deleted),
        PurgePlan::Noop => Planned::Finish(DeleteResult::Deleted),
        PurgePlan::Conflict(conflict) => Planned::Finish(DeleteResult::Conflict(conflict)),
    }
}

/// Most read → plan → conditional-write attempts one call makes before giving
/// up on a key that other writers keep changing underneath it.
const MAX_WRITE_ATTEMPTS: usize = 8;

/// Result of one attempt: finished with an answer, or lost the race (412) and
/// must re-read and re-plan.
#[derive(Debug, PartialEq, Eq)]
enum Step<T> {
    Done(T),
    Retry,
}

/// Run `attempt` until it finishes, at most [`MAX_WRITE_ATTEMPTS`] times. An
/// error from an attempt ends the loop immediately. Each retry re-reads and
/// re-plans, so the planner always decides against the true current record,
/// just as it does under the fs and git stores' locks.
async fn retry_on_lost_race<T, F, Fut>(key: &RecordKey, mut attempt: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Step<T>>>,
{
    for _ in 0..MAX_WRITE_ATTEMPTS {
        if let Step::Done(value) = attempt().await? {
            return Ok(value);
        }
    }
    Err(CoreError::Backend(format!(
        "s3: conditional write for {key} lost {MAX_WRITE_ATTEMPTS} consecutive races"
    )))
}

/// Result of one conditional S3 write: it applied, or its precondition failed
/// (412) because a concurrent writer changed the object after our read.
#[derive(Debug, PartialEq, Eq)]
enum WriteOutcome {
    Applied,
    PreconditionFailed,
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
    // ---- consumer surface (tombstones hidden) ----

    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        Ok(visible(self.read(key).await?))
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        // Spec §8.4: a tombstone lives at the record's normal object key, so the
        // listing alone can't tell it apart from a live record. Hiding tombstones
        // costs one GetObject per key, on top of the ListObjectsV2 pages. That's
        // expensive for large namespaces, but acceptable at current sizes and
        // tracked as a follow-up (a kind marker in the key suffix, or a
        // per-collection tombstone index; both are layout changes needing their
        // own design). Don't optimise it here.
        let mut out = Vec::new();
        for key in self.list_keys(prefix).await? {
            if listed_as_live(self.read(&key).await)? {
                out.push(key);
            }
        }
        Ok(out)
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // Consumer write: a tombstone counts as absent, so `expected = None`
        // over one is a recreation that re-stamps the revision (`plan_put`).
        let key = record.key.clone();
        self.write_planned(&key, |current| {
            put_step(
                &key,
                plan_put(current, record.clone(), expected.clone(), self.cap),
            )
        })
        .await
    }

    // `delete` is the trait's provided method: `delete_as(key, expected, None)`.
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult> {
        // Deletion writes a tombstone (spec §3.1). `now_ms()` is taken per
        // attempt, so a retried delete stamps when it actually landed.
        self.write_planned(key, |current| {
            Ok(delete_step(plan_delete(
                current,
                expected.clone(),
                now_ms(),
                self.cap,
                author.as_ref(),
            )))
        })
        .await
    }

    // ---- replication surface (tombstones visible) ----

    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.read(key).await
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        self.list_keys(prefix).await
    }

    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // Replication write: stores the caller's revision verbatim, never
        // re-stamps, and a tombstone is a real current record it must name in
        // `expected` (`plan_put_raw`).
        let key = record.key.clone();
        self.write_planned(&key, |current| {
            put_step(
                &key,
                plan_put_raw(current, record.clone(), expected.clone(), self.cap),
            )
        })
        .await
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        // The only physical removal: a conditional DeleteObject, and only when
        // the stored revision is still `expected`, so a recreation that lands
        // mid-purge survives.
        self.write_planned(key, |current| {
            Ok(purge_step(plan_purge(current, &expected)))
        })
        .await
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

    use gonzalo_core::store::Conflict;
    use gonzalo_core::{Body, Identity, Meta, RecordKind, plan_put, tombstone_of};
    use std::cell::Cell;
    use std::collections::BTreeMap;

    fn live(key: &RecordKey, payload: &[u8]) -> Record {
        Record {
            key: key.clone(),
            kind: RecordKind::Topic,
            revision: Revision::initial(payload),
            parent: None,
            body: Body::Inline(payload.to_vec()),
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

    fn tomb(key: &RecordKey, payload: &[u8]) -> Record {
        tombstone_of(&live(key, payload), 1_000, DEFAULT_ANCESTOR_CAP, None)
    }

    fn conflict(key: &RecordKey) -> Box<Conflict> {
        Box::new(Conflict {
            key: key.clone(),
            expected: Some(Revision::initial(b"stale")),
            current: live(key, b"current"),
        })
    }

    /// An `S3Store` that never touches the network: building a client does no
    /// I/O, so the builder can be unit-tested without an endpoint.
    fn offline_store() -> S3Store {
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build();
        S3Store::new(Client::from_conf(conf), "offline")
    }

    // ---- preconditions ----

    #[test]
    fn create_uses_if_absent() {
        // No object was read → create-only.
        assert_eq!(precondition(None), Precondition::IfAbsent);
    }

    #[test]
    fn update_uses_if_match_on_the_read_etag() {
        assert_eq!(
            precondition(Some("\"abc123\"")),
            Precondition::IfMatch("\"abc123\"".to_string())
        );
    }

    #[test]
    fn recreation_over_tombstone_uses_if_match() {
        // `put(record, None)` over a tombstone is a recreation the planner
        // writes. The tombstone object exists, so the write must be `If-Match`
        // on its ETag. `If-None-Match: *` would 412 on every attempt.
        let k = RecordKey::new("ns", "col", "recreate");
        let plan = plan_put(
            Some(&tomb(&k, b"old")),
            live(&k, b"new"),
            None,
            DEFAULT_ANCESTOR_CAP,
        );
        assert!(matches!(plan, PutPlan::Write(_)));
        assert_eq!(
            precondition(Some("\"tomb-etag\"")),
            Precondition::IfMatch("\"tomb-etag\"".to_string())
        );
    }

    // ---- consumer read filtering ----

    #[test]
    fn visible_hides_tombstones_and_keeps_live() {
        let k = RecordKey::new("ns", "col", "vis");
        assert_eq!(visible(None), None);
        assert_eq!(visible(Some(tomb(&k, b"x"))), None);
        let rec = live(&k, b"x");
        assert_eq!(visible(Some(rec.clone())), Some(rec));
    }

    #[test]
    fn list_filter_excludes_tombstones_and_vanished_keys() {
        let k = RecordKey::new("ns", "col", "listed");
        assert!(listed_as_live(Ok(Some(live(&k, b"x")))).unwrap());
        assert!(!listed_as_live(Ok(Some(tomb(&k, b"x")))).unwrap());
        // Purged between the listing and the read.
        assert!(!listed_as_live(Ok(None)).unwrap());
    }

    #[test]
    fn list_filter_keeps_undecodable_objects_and_propagates_backend_errors() {
        // An object that fails to decode stays listed; `get` surfaces the error.
        // Any other read error fails the list (s3 rule; fs and git keep such
        // entries listed).
        assert!(listed_as_live(Err(CoreError::Serde("bad json".into()))).unwrap());
        assert!(matches!(
            listed_as_live(Err(CoreError::Backend("503".into()))),
            Err(CoreError::Backend(_))
        ));
    }

    // ---- plan → S3 action ----

    #[test]
    fn put_step_writes_and_commits_the_planned_revision() {
        let k = RecordKey::new("ns", "col", "put-write");
        let rec = live(&k, b"v");
        match put_step(&k, PutPlan::Write(rec.clone())) {
            Ok(Planned::Put(stored, answer)) => {
                assert_eq!(answer, PutResult::Committed(rec.revision.clone()));
                assert_eq!(stored, rec);
            }
            other => panic!("expected Put, got {other:?}"),
        }
    }

    #[test]
    fn put_step_finishes_on_conflict() {
        let k = RecordKey::new("ns", "col", "put-conflict");
        match put_step(&k, PutPlan::Conflict(conflict(&k))) {
            Ok(Planned::Finish(PutResult::Conflict(c))) => assert_eq!(c, conflict(&k)),
            other => panic!("expected Finish(Conflict), got {other:?}"),
        }
    }

    #[test]
    fn put_step_maps_not_found_to_error() {
        let k = RecordKey::new("ns", "col", "put-missing");
        match put_step(&k, PutPlan::NotFound) {
            Err(CoreError::NotFound(got)) => assert_eq!(got, k),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn put_step_maps_rejected_to_an_invalid_call() {
        // `Invalid`, not `Backend`: the call can never succeed, and a daemon in
        // front of this store answers 400 rather than 500 (#299).
        let k = RecordKey::new("ns", "col", "put-rejected");
        let reason = gonzalo_core::CONSUMER_TOMBSTONE_REJECTED;
        match put_step(&k, PutPlan::Rejected(reason)) {
            Err(CoreError::Invalid(msg)) => assert_eq!(msg, reason),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn delete_step_maps_each_plan() {
        let k = RecordKey::new("ns", "col", "del");
        let t = tomb(&k, b"x");
        assert_eq!(
            delete_step(DeletePlan::Write(t.clone())),
            Planned::Put(t, DeleteResult::Deleted)
        );
        assert_eq!(
            delete_step(DeletePlan::Noop),
            Planned::Finish(DeleteResult::Deleted)
        );
        assert_eq!(
            delete_step(DeletePlan::Conflict(conflict(&k))),
            Planned::Finish(DeleteResult::Conflict(conflict(&k)))
        );
    }

    #[test]
    fn purge_step_maps_each_plan() {
        let k = RecordKey::new("ns", "col", "purge");
        assert_eq!(
            purge_step(PurgePlan::Remove),
            Planned::Remove(DeleteResult::Deleted)
        );
        assert_eq!(
            purge_step(PurgePlan::Noop),
            Planned::Finish(DeleteResult::Deleted)
        );
        assert_eq!(
            purge_step(PurgePlan::Conflict(conflict(&k))),
            Planned::Finish(DeleteResult::Conflict(conflict(&k)))
        );
    }

    // ---- lost-race classification ----

    #[test]
    fn lost_race_covers_412_409_and_vanished_objects() {
        assert!(is_lost_race(Some("PreconditionFailed")));
        assert!(is_lost_race(Some("ConditionalRequestConflict")));
        assert!(is_lost_race(Some("NoSuchKey")));
        assert!(!is_lost_race(Some("AccessDenied")));
        assert!(!is_lost_race(None));
    }

    // ---- lost-race retry loop ----

    #[tokio::test]
    async fn retry_gives_up_after_max_attempts() {
        let k = RecordKey::new("ns", "col", "hot");
        let calls = Cell::new(0usize);
        let calls_ref = &calls;
        let out: Result<()> = retry_on_lost_race(&k, move || async move {
            calls_ref.set(calls_ref.get() + 1);
            Ok(Step::Retry)
        })
        .await;
        assert_eq!(calls.get(), MAX_WRITE_ATTEMPTS);
        assert_eq!(MAX_WRITE_ATTEMPTS, 8);
        match out {
            Err(CoreError::Backend(msg)) => assert_eq!(
                msg,
                format!("s3: conditional write for {k} lost 8 consecutive races")
            ),
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retry_returns_as_soon_as_an_attempt_lands() {
        // Losing every attempt but the last still succeeds, with no extra call.
        let k = RecordKey::new("ns", "col", "warm");
        let calls = Cell::new(0usize);
        let calls_ref = &calls;
        let out = retry_on_lost_race(&k, move || async move {
            calls_ref.set(calls_ref.get() + 1);
            if calls_ref.get() < MAX_WRITE_ATTEMPTS {
                Ok(Step::Retry)
            } else {
                Ok(Step::Done("landed"))
            }
        })
        .await;
        assert_eq!(out.unwrap(), "landed");
        assert_eq!(calls.get(), MAX_WRITE_ATTEMPTS);
    }

    #[tokio::test]
    async fn retry_propagates_errors_without_retrying() {
        let k = RecordKey::new("ns", "col", "broken");
        let calls = Cell::new(0usize);
        let calls_ref = &calls;
        let out: Result<()> = retry_on_lost_race(&k, move || async move {
            calls_ref.set(calls_ref.get() + 1);
            Err(CoreError::Backend("access denied".into()))
        })
        .await;
        assert!(matches!(out, Err(CoreError::Backend(m)) if m == "access denied"));
        assert_eq!(calls.get(), 1);
    }

    // ---- ancestor cap ----

    #[tokio::test]
    async fn new_store_uses_default_ancestor_cap() {
        assert_eq!(offline_store().cap, DEFAULT_ANCESTOR_CAP);
    }

    #[tokio::test]
    async fn with_ancestor_cap_sets_cap() {
        match offline_store().with_ancestor_cap(3) {
            Ok(store) => assert_eq!(store.cap, 3),
            Err(e) => panic!("cap 3 must be accepted: {e}"),
        }
    }

    #[tokio::test]
    async fn with_ancestor_cap_rejects_zero() {
        assert!(matches!(
            offline_store().with_ancestor_cap(0),
            Err(CoreError::Backend(_))
        ));
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
