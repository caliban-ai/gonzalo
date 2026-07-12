//! Filesystem storage substrate for gonzalo.

mod layout;

use async_trait::async_trait;
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DeleteResult, KeyPrefix, PutResult, Record, RecordKey,
    Result, Revision, Store, store::Conflict,
};
use rustix::fs::{FlockOperation, flock};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt;

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

    async fn delete(&self, key: &RecordKey, expected: Option<Revision>) -> Result<DeleteResult> {
        // Mirror `put`'s critical section: hold the per-record flock so the
        // read→check→remove is atomic against a concurrent writer. Blocking, so
        // run it on a blocking thread rather than stalling the async runtime.
        let root = self.root.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || delete_locked(&root, &key, expected))
            .await
            .map_err(|e| CoreError::Backend(format!("delete task panicked: {e}")))?
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
        // Durable publish: write the temp file and `sync_all` it so its bytes
        // reach disk BEFORE the rename, then fsync the parent directory AFTER
        // the rename so the new directory entry survives a crash too. `rename`
        // is atomic against concurrent readers but not against power loss — on
        // ext4 delayed allocation a crash just after a reported success can
        // otherwise leave a zero-length or truncated blob.
        let mut f = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        f.write_all(content)
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        f.sync_all()
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        drop(f);
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        if let Some(parent) = path.parent() {
            let parent = parent.to_path_buf();
            tokio::task::spawn_blocking(move || fsync_dir(&parent))
                .await
                .map_err(|e| CoreError::Backend(format!("fsync task panicked: {e}")))?
                .map_err(|e| CoreError::Backend(e.to_string()))?;
        }
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
    // Durable atomic write: write the temp file and `sync_all` it so its bytes
    // reach disk BEFORE the rename, then fsync the parent directory AFTER the
    // rename so the new directory entry survives a crash too. `rename` is atomic
    // against concurrent readers but not against power loss — on ext4 delayed
    // allocation a crash just after a reported Committed can otherwise leave a
    // zero-length or truncated record.
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::File::create(&tmp).map_err(|e| CoreError::Backend(e.to_string()))?;
    f.write_all(&bytes)
        .map_err(|e| CoreError::Backend(e.to_string()))?;
    f.sync_all()
        .map_err(|e| CoreError::Backend(e.to_string()))?;
    drop(f);
    std::fs::rename(&tmp, &path).map_err(|e| CoreError::Backend(e.to_string()))?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent).map_err(|e| CoreError::Backend(e.to_string()))?;
    }
    Ok(PutResult::Committed(record.revision))
}

/// Perform the conditional `delete` under the same per-record advisory lock
/// `put_locked` uses, so the read→check→remove is atomic against a concurrent
/// writer. Blocking by design; call from `spawn_blocking`.
///
/// `expected == None` removes the record if present (idempotent no-op if
/// absent). `expected == Some(rev)` removes only if the current revision matches;
/// a mismatch is a `Conflict`, and an already-absent key is an idempotent
/// `Deleted` (the revision is already gone — nothing to conflict on). We leave
/// the sibling `.lock` file in place (it is reused by the next writer).
fn delete_locked(root: &Path, key: &RecordKey, expected: Option<Revision>) -> Result<DeleteResult> {
    let path = layout::record_path(root, key);
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

    // Critical section: revision check and removal are now serialized per record.
    let current = match std::fs::read(&path) {
        Ok(bytes) => Some(
            serde_json::from_slice::<Record>(&bytes)
                .map_err(|e| CoreError::Serde(e.to_string()))?,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(CoreError::Backend(e.to_string())),
    };

    match (current, &expected) {
        // Absent: nothing to remove. Idempotent `Deleted` regardless of
        // `expected` — the revision the caller named is already gone.
        (None, _) => Ok(DeleteResult::Deleted),
        // Unconditional, or the expected revision matches: remove the record.
        (Some(cur), exp) if exp.is_none() || exp.as_ref() == Some(&cur.revision) => {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                // A concurrent remover won under the lock hand-off — still absent.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(CoreError::Backend(e.to_string())),
            }
            if let Some(parent) = path.parent() {
                fsync_dir(parent).map_err(|e| CoreError::Backend(e.to_string()))?;
            }
            Ok(DeleteResult::Deleted)
        }
        // Present but the expected revision differs: surface a Conflict.
        (Some(cur), _) => Ok(DeleteResult::Conflict(Box::new(Conflict {
            key: key.clone(),
            expected,
            current: cur,
        }))),
    }
}

/// Best-effort fsync of the directory `path`, making a preceding `rename` into
/// it durable across a crash. A `rename` is atomic against concurrent readers,
/// but on power loss the new directory entry can still be lost until the parent
/// directory's own metadata is flushed. Where a platform rejects fsync on a
/// directory handle (surfaced as `EINVAL`/`InvalidInput`), treat it as a no-op
/// rather than a write failure.
fn fsync_dir(path: &Path) -> io::Result<()> {
    let dir = std::fs::File::open(path)?;
    match dir.sync_all() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => Ok(()),
        Err(e) => Err(e),
    }
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
                    // Directory/file names are `segment`-encoded; decode each
                    // component back to the original key so `list()` round-trips.
                    let key = RecordKey::new(
                        gonzalo_core::decode_segment(&ns_name),
                        gonzalo_core::decode_segment(&col_name),
                        gonzalo_core::decode_segment(id),
                    );
                    if prefix.matches(&key) {
                        out.push(key);
                    }
                }
            }
        }
    }
    Ok(())
}
