//! Filesystem storage substrate for gonzalo.

mod layout;

use async_trait::async_trait;
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, KeyPrefix, PutResult, Record, RecordKey, Result, Revision,
    Store, store::Conflict,
};
use rustix::fs::{FlockOperation, flock};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A `Store` backed by JSON files under a root directory.
pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    async fn read_record(&self, key: &RecordKey) -> Result<Option<Record>> {
        let path = layout::record_path(&self.root, key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let rec: Record =
                    serde_json::from_slice(&bytes).map_err(|e| CoreError::Serde(e.to_string()))?;
                Ok(Some(rec))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CoreError::Backend(e.to_string())),
        }
    }
}

#[async_trait]
impl Store for FsStore {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.read_record(key).await
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // The OCC read-check-write-rename is a critical section: without
        // serialization a concurrent writer can commit between our read and our
        // rename, silently losing an update. Hold a per-record advisory file
        // lock (flock) across the whole section so writers — in this process or
        // another — serialize. flock is blocking, so run it on a blocking
        // thread rather than stalling the async runtime.
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || put_locked(&root, record, expected))
            .await
            .map_err(|e| CoreError::Backend(format!("put task panicked: {e}")))?
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        let mut out = Vec::new();
        collect_keys(&self.root, prefix, &mut out).await?;
        Ok(out)
    }
}

/// Process-unique nonce for blob temp files, so concurrent writers never share
/// a temp path (see `put_blob`).
static BLOB_TMP_NONCE: AtomicU64 = AtomicU64::new(0);

#[async_trait]
impl BlobStore for FsStore {
    async fn put_blob(&self, content: &[u8]) -> Result<ContentHash> {
        let hash = ContentHash::of(content);
        let path = layout::blob_path(&self.root, &hash);

        // Write-if-absent: identical content hashes to the same path, so an
        // existing blob is already exactly these bytes — nothing to do.
        if tokio::fs::try_exists(&path)
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))?
        {
            return Ok(hash);
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| CoreError::Backend(e.to_string()))?;
        }

        // Atomic publish: write a process-unique temp, then rename into place.
        // Content-addressing makes a same-content race benign (byte-identical),
        // and the unique temp keeps two racing writers from clobbering one temp.
        let nonce = BLOB_TMP_NONCE.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp.{}.{nonce}", std::process::id()));
        tokio::fs::write(&tmp, content)
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        Ok(hash)
    }

    async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        let path = layout::blob_path(&self.root, hash);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CoreError::Backend(e.to_string())),
        }
    }

    async fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        let dir = layout::blobs_dir(&self.root);
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            // No blobs dir yet == no blobs.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(CoreError::Backend(e.to_string())),
        };
        let mut out = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))?
        {
            let name = entry.file_name().to_string_lossy().to_string();
            // A committed blob's filename is exactly its blake3 hex hash. In-flight
            // temp files (`<hash>.tmp.<pid>.<nonce>`) and any stray files carry a
            // `.` and are skipped, so a concurrent `put_blob` is never mistaken for
            // a collectable blob.
            if is_blob_hash(&name) {
                out.push(ContentHash(name));
            }
        }
        Ok(out)
    }

    async fn delete_blob(&self, hash: &ContentHash) -> Result<()> {
        let path = layout::blob_path(&self.root, hash);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            // Idempotent: an already-absent blob is a successful no-op.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CoreError::Backend(e.to_string())),
        }
    }
}

/// Whether `name` is a committed blob's filename: blake3 hex, `[0-9a-f]{64}`.
/// Excludes in-flight temp files and any stray non-blob entries.
fn is_blob_hash(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Perform the conditional `put` under a per-record advisory lock. Blocking by
/// design (held across read→check→write→rename); call from `spawn_blocking`.
///
/// The lock is a sibling `<id>.json.lock` file held exclusively via `flock`,
/// released when `lock` drops. It guards only writers — `get`/`list` stay
/// lock-free — which is sufficient: the lost update is a write/write race, and
/// the final `rename` is atomic so readers never observe a torn file.
fn put_locked(root: &Path, record: Record, expected: Option<Revision>) -> Result<PutResult> {
    let path = layout::record_path(root, &record.key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CoreError::Backend(e.to_string()))?;
    }

    // Acquire the exclusive lock; it lives until `lock` drops at function end.
    let lock_path = path.with_extension("json.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| CoreError::Backend(e.to_string()))?;
    flock(&lock, FlockOperation::LockExclusive).map_err(|e| CoreError::Backend(e.to_string()))?;

    // Critical section: revision check and write are now serialized per record.
    let current = match std::fs::read(&path) {
        Ok(bytes) => Some(
            serde_json::from_slice::<Record>(&bytes)
                .map_err(|e| CoreError::Serde(e.to_string()))?,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(CoreError::Backend(e.to_string())),
    };
    let current_rev = current.as_ref().map(|r| r.revision.clone());
    if current_rev != expected {
        if let Some(current) = current {
            return Ok(PutResult::Conflict(Box::new(Conflict {
                key: record.key.clone(),
                expected,
                current,
            })));
        }
        // expected referenced a revision but nothing exists: treat as conflict
        return Err(CoreError::NotFound(record.key.clone()));
    }

    let bytes = serde_json::to_vec_pretty(&record).map_err(|e| CoreError::Serde(e.to_string()))?;
    // Atomic write: temp file + rename.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| CoreError::Backend(e.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|e| CoreError::Backend(e.to_string()))?;
    Ok(PutResult::Committed(record.revision))
}

/// Walk `<root>/<ns>/<col>/<id>.json` and collect keys matching `prefix`.
async fn collect_keys(
    root: &std::path::Path,
    prefix: &KeyPrefix,
    out: &mut Vec<RecordKey>,
) -> Result<()> {
    let mut namespaces = match tokio::fs::read_dir(root).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CoreError::Backend(e.to_string())),
    };
    while let Some(ns) = namespaces
        .next_entry()
        .await
        .map_err(|e| CoreError::Backend(e.to_string()))?
    {
        if ns.file_type().await.map(|ft| !ft.is_dir()).unwrap_or(true) {
            continue;
        }
        let ns_name = ns.file_name().to_string_lossy().to_string();
        let mut cols = tokio::fs::read_dir(ns.path())
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        while let Some(col) = cols
            .next_entry()
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))?
        {
            if col.file_type().await.map(|ft| !ft.is_dir()).unwrap_or(true) {
                continue;
            }
            let col_name = col.file_name().to_string_lossy().to_string();
            let mut files = tokio::fs::read_dir(col.path())
                .await
                .map_err(|e| CoreError::Backend(e.to_string()))?;
            while let Some(f) = files
                .next_entry()
                .await
                .map_err(|e| CoreError::Backend(e.to_string()))?
            {
                let fname = f.file_name().to_string_lossy().to_string();
                if let Some(id) = fname.strip_suffix(".json") {
                    let key = RecordKey::new(ns_name.clone(), col_name.clone(), id.to_string());
                    if prefix.matches(&key) {
                        out.push(key);
                    }
                }
            }
        }
    }
    Ok(())
}
