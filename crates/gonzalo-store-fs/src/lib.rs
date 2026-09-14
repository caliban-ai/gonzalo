//! Filesystem storage substrate for gonzalo.

mod layout;
mod tilde;

pub use tilde::expand_tilde;

use async_trait::async_trait;
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DEFAULT_ANCESTOR_CAP, DeletePlan, DeleteResult, Identity,
    KeyPrefix, PurgePlan, PutPlan, PutResult, Record, RecordKey, Result, Revision, Store, now_ms,
    plan_delete, plan_purge, plan_put, plan_put_raw, validate_ancestor_cap,
};
use rustix::fs::{FlockOperation, flock};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt;

/// A put planner: `gonzalo_core::plan_put` (consumer write) or
/// `gonzalo_core::plan_put_raw` (replication write). Both run inside the same
/// locked read→plan→write path, `put_locked`.
type PutPlanner = fn(Option<&Record>, Record, Option<Revision>, usize) -> PutPlan;

/// A `Store` backed by JSON files under a root directory.
pub struct FsStore {
    root: PathBuf,
    /// Upper bound on `Record::ancestors` for every record this store writes
    /// (spec §3.9). Defaults to `DEFAULT_ANCESTOR_CAP`.
    cap: usize,
}

impl FsStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cap: DEFAULT_ANCESTOR_CAP,
        }
    }

    /// Bound the ancestor list this store keeps on every write (spec §3.9).
    /// A cap of `0` is rejected.
    pub fn with_ancestor_cap(mut self, cap: usize) -> Result<Self> {
        self.cap = validate_ancestor_cap(cap)?;
        Ok(self)
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

    /// Whether consumer `list` reports `key`. A tombstone is hidden. A file
    /// that vanished since the directory walk (a concurrent `purge`) is
    /// dropped. A file that fails to read for any other reason (fails to
    /// deserialize, or an unreadable entry such as a stray directory named
    /// `*.json`) stays listed, exactly as before tombstones, so `get` keeps
    /// surfacing the error instead of the key silently disappearing.
    async fn listed_live(&self, key: &RecordKey) -> bool {
        match self.read_record(key).await {
            Ok(Some(rec)) => !rec.is_tombstone(),
            Ok(None) => false,
            Err(_) => true,
        }
    }
}

#[async_trait]
impl Store for FsStore {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        // Consumer read: a tombstoned key looks absent (spec §3.2).
        Ok(self
            .read_record(key)
            .await?
            .filter(|rec| !rec.is_tombstone()))
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // The OCC read-check-write-rename is a critical section: without
        // serialization a concurrent writer can commit between our read and our
        // rename, silently losing an update. Hold a per-record advisory file
        // lock (flock) across the whole section so writers — in this process or
        // another — serialize. flock is blocking, so run it on a blocking
        // thread rather than stalling the async runtime.
        let root = self.root.clone();
        let cap = self.cap;
        tokio::task::spawn_blocking(move || put_locked(&root, record, expected, cap, plan_put))
            .await
            .map_err(|e| CoreError::Backend(format!("put task panicked: {e}")))?
    }

    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // Replication write: the same per-record flock critical section as
        // `put`, decided by `plan_put_raw`, which never re-stamps. A create
        // (`expected == None`) over a tombstone is a Conflict, never a
        // recreation.
        let root = self.root.clone();
        let cap = self.cap;
        tokio::task::spawn_blocking(move || put_locked(&root, record, expected, cap, plan_put_raw))
            .await
            .map_err(|e| CoreError::Backend(format!("put_raw task panicked: {e}")))?
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        // Consumer listing excludes tombstoned keys, which means reading each
        // record file under the prefix (spec §8.4: a local read per key on fs).
        let mut keys = Vec::new();
        collect_keys(&self.root, prefix, &mut keys).await?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if self.listed_live(&key).await {
                out.push(key);
            }
        }
        Ok(out)
    }

    // No `delete` here: the trait provides it as `delete_as(key, expected, None)`.
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult> {
        // Mirror `put`'s critical section: hold the per-record flock so the
        // read→plan→tombstone-write is atomic against a concurrent writer.
        // Blocking, so run it on a blocking thread rather than stalling the
        // async runtime.
        let root = self.root.clone();
        let key = key.clone();
        let cap = self.cap;
        tokio::task::spawn_blocking(move || delete_locked(&root, &key, expected, cap, author))
            .await
            .map_err(|e| CoreError::Backend(format!("delete task panicked: {e}")))?
    }

    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        // Replication read: returns whatever is stored, tombstones included.
        self.read_record(key).await
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        // Replication listing: every `<id>.json` under the prefix, tombstones
        // included, without reading any file.
        let mut out = Vec::new();
        collect_keys(&self.root, prefix, &mut out).await?;
        Ok(out)
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        // The only physical removal in the system. It runs in the same
        // per-record flock critical section as `put`, so a concurrent
        // recreation either lands before our read (→ Conflict) or after our
        // unlink (→ a fresh record). Blocking, so run it on a blocking thread.
        let root = self.root.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || purge_locked(&root, &key, &expected))
            .await
            .map_err(|e| CoreError::Backend(format!("purge task panicked: {e}")))?
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

/// Take the exclusive per-record advisory lock for the record file at `path`,
/// creating its collection directory first. Blocking by design; call from
/// `spawn_blocking`.
///
/// The lock is a sibling `<id>.json.lock` file held exclusively via `flock`,
/// released when the returned handle drops. It guards only writers — `get`/
/// `list` stay lock-free — which is sufficient: the lost update is a
/// write/write race, and every publish is an atomic `rename` (or `unlink`), so
/// readers never observe a torn file. The `.lock` file is left in place and
/// reused by the next writer.
fn lock_record(path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CoreError::Backend(e.to_string()))?;
    }
    let lock_path = path.with_extension("json.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| CoreError::Backend(e.to_string()))?;
    flock(&lock, FlockOperation::LockExclusive).map_err(|e| CoreError::Backend(e.to_string()))?;
    Ok(lock)
}

/// The record stored at `path` right now, tombstones included. Called inside
/// the caller's critical section.
fn read_current(path: &Path) -> Result<Option<Record>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Record>(&bytes)
            .map(Some)
            .map_err(|e| CoreError::Serde(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CoreError::Backend(e.to_string())),
    }
}

/// Durably and atomically publish `record` at `path`. Called under
/// `lock_record`.
fn write_durable(path: &Path, record: &Record) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(record).map_err(|e| CoreError::Serde(e.to_string()))?;
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
    std::fs::rename(&tmp, path).map_err(|e| CoreError::Backend(e.to_string()))?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent).map_err(|e| CoreError::Backend(e.to_string()))?;
    }
    Ok(())
}

/// Durably unlink the record file at `path`: remove it, then fsync the parent
/// directory so the removal survives a crash. Called under `lock_record`.
fn remove_durable(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        // A concurrent remover won under the lock hand-off — still absent.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(CoreError::Backend(e.to_string())),
    }
    if let Some(parent) = path.parent() {
        fsync_dir(parent).map_err(|e| CoreError::Backend(e.to_string()))?;
    }
    Ok(())
}

/// Perform a conditional `put` or `put_raw` under the per-record lock
/// (`lock_record`). Blocking by design (held across read→plan→write→rename);
/// call from `spawn_blocking`.
///
/// The decision is the planner's: `plan_put` for consumer writes, which
/// re-stamps a recreation over a tombstone, or `plan_put_raw` for
/// replication writes, which never re-stamps. Both fold ancestors to `cap`, so
/// every substrate behaves identically (spec §3.2). This function only carries
/// the plan out.
fn put_locked(
    root: &Path,
    record: Record,
    expected: Option<Revision>,
    cap: usize,
    plan: PutPlanner,
) -> Result<PutResult> {
    let key = record.key.clone();
    let path = layout::record_path(root, &key);
    // Acquire the exclusive lock; it lives until `_lock` drops at function end.
    let _lock = lock_record(&path)?;

    // Critical section: the read, the decision and the write are serialized
    // per record.
    let current = read_current(&path)?;
    match plan(current.as_ref(), record, expected, cap) {
        PutPlan::Write(stored) => {
            write_durable(&path, &stored)?;
            Ok(PutResult::Committed(stored.revision))
        }
        PutPlan::Conflict(conflict) => Ok(PutResult::Conflict(conflict)),
        // `expected` named a revision, but nothing live (and no tombstone at
        // that revision) is stored.
        PutPlan::NotFound => Err(CoreError::NotFound(key)),
        // Only consumer `plan_put` produces this, for a `RecordKind::Tombstone`
        // record: deletes go through `delete_as`, replication through `put_raw`.
        PutPlan::Rejected(reason) => Err(CoreError::Backend(reason.to_string())),
    }
}

/// Perform the conditional `purge` under the per-record lock: physically
/// unlink the record file (live or tombstone) only if its current revision is
/// `expected` (`gonzalo_core::plan_purge`). This is the only physical removal
/// of a record file. Blocking; call from `spawn_blocking`.
fn purge_locked(root: &Path, key: &RecordKey, expected: &Revision) -> Result<DeleteResult> {
    let path = layout::record_path(root, key);
    // Acquire the exclusive lock; it lives until `_lock` drops at function end.
    let _lock = lock_record(&path)?;

    // Critical section: the revision check and the unlink are serialized per
    // record.
    let current = read_current(&path)?;
    match plan_purge(current.as_ref(), expected) {
        PurgePlan::Remove => {
            remove_durable(&path)?;
            Ok(DeleteResult::Deleted)
        }
        PurgePlan::Noop => Ok(DeleteResult::Deleted),
        PurgePlan::Conflict(conflict) => Ok(DeleteResult::Conflict(conflict)),
    }
}

/// Perform the conditional `delete` under the per-record lock. Blocking by
/// design; call from `spawn_blocking`.
///
/// A delete no longer removes anything (spec §3.3). Over a live record it
/// durably publishes the tombstone `gonzalo_core::plan_delete` builds, at the
/// record's normal path, through the same temp+fsync+rename as `put`. Over an
/// absent key or an existing tombstone it writes nothing and reports `Deleted`.
/// A stale `expected` over a live record is a `Conflict`. `Some(author)`
/// replaces `meta.author` on the tombstone. Physical removal is
/// `purge_locked`'s job alone.
fn delete_locked(
    root: &Path,
    key: &RecordKey,
    expected: Option<Revision>,
    cap: usize,
    author: Option<Identity>,
) -> Result<DeleteResult> {
    let path = layout::record_path(root, key);
    // Acquire the exclusive lock; it lives until `_lock` drops at function end.
    let _lock = lock_record(&path)?;

    // Critical section: the read, the decision and the tombstone write are
    // serialized per record.
    let current = read_current(&path)?;
    match plan_delete(current.as_ref(), expected, now_ms(), cap, author.as_ref()) {
        DeletePlan::Write(tombstone) => {
            write_durable(&path, &tombstone)?;
            Ok(DeleteResult::Deleted)
        }
        DeletePlan::Noop => Ok(DeleteResult::Deleted),
        DeletePlan::Conflict(conflict) => Ok(DeleteResult::Conflict(conflict)),
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
