//! Git-backed storage substrate. Each record is a JSON file in a git
//! worktree; every write is committed, giving an auditable history. Remote
//! replication via fast-forward `pull`/`push`.
//!
//! [`GitStore`] implements [`Store`](gonzalo_core::Store) but **not**
//! [`BlobStore`](gonzalo_core::BlobStore): git is not a natural content-addressed
//! blob store. Blob-backed records (e.g. checkpoint pre-images, code-graph
//! slices) therefore require the `fs`, `s3`, or remote (daemon) substrates, not
//! git (gonzalo#184).

use async_trait::async_trait;
use gonzalo_core::{
    Body, CoreError, DEFAULT_ANCESTOR_CAP, DeletePlan, DeleteResult, Identity, KeyPrefix,
    MergeOutcome, PurgePlan, PutPlan, PutResult, Record, RecordKey, Result, Revision,
    decode_segment, merge, now_ms, plan_delete, plan_purge, plan_put, plan_put_raw,
    reconciled_record, record_components, tombstone_winner, validate_ancestor_cap,
};
use rustix::fs::{FlockOperation, flock};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

mod diff;
pub use diff::{ChangedPaths, changed_paths, head_commit, is_git_repo};

/// A put planner: `gonzalo_core::plan_put` (consumer write) or
/// `gonzalo_core::plan_put_raw` (replication write). Both run inside the same
/// locked read→plan→write→commit path, `GitStore::put_locked`.
type PutPlanner = fn(Option<&Record>, Record, Option<Revision>, usize) -> PutPlan;

/// A record that diverged on both sides of a pull and could not be auto-merged;
/// the local version is kept and both sides are surfaced for resolution.
#[derive(Debug, Clone)]
pub struct PullConflict {
    pub key: RecordKey,
    pub local: Box<Record>,
    pub remote: Box<Record>,
}

/// What a [`GitStore::pull`] did.
#[derive(Debug, Default)]
#[must_use = "a PullReport may contain unresolved conflicts that must be handled"]
pub struct PullReport {
    /// The pull was a clean fast-forward (or already up-to-date).
    pub fast_forwarded: bool,
    /// Records reconciled by a content-aware 3-way merge into a merge commit.
    pub merged: Vec<RecordKey>,
    /// Divergences kept local and surfaced for the caller to resolve.
    pub conflicts: Vec<PullConflict>,
}

pub struct GitStore {
    root: PathBuf,
    /// Upper bound on `Record::ancestors` for every record this store commits
    /// (spec §3.9). Defaults to `DEFAULT_ANCESTOR_CAP`.
    cap: usize,
}

impl GitStore {
    /// Open an existing git repo at `root`, or initialize one if absent.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| CoreError::Backend(e.to_string()))?;
        match git2::Repository::open(&root) {
            Ok(_) => {}
            Err(_) => {
                git2::Repository::init(&root).map_err(|e| CoreError::Backend(e.to_string()))?;
            }
        }
        Ok(Self {
            root,
            cap: DEFAULT_ANCESTOR_CAP,
        })
    }

    /// Bound the ancestor list this store keeps on every write (spec §3.9).
    /// A cap of `0` is rejected.
    pub fn with_ancestor_cap(mut self, cap: usize) -> Result<Self> {
        self.cap = validate_ancestor_cap(cap)?;
        Ok(self)
    }

    /// An owned copy of this handle, to move into a `spawn_blocking` closure.
    fn handle(&self) -> Self {
        Self {
            root: self.root.clone(),
            cap: self.cap,
        }
    }

    /// Perform a conditional `put` or `put_raw`: take the repo lock, read the
    /// current record, let `plan` (`plan_put` or `plan_put_raw`) decide, and
    /// commit the planned record. Serializes the read→plan→write→commit
    /// critical section over the shared index+HEAD; the lock releases when
    /// `_lock` drops (all paths). Blocking; call from `run_blocking`.
    fn put_locked(
        &self,
        record: Record,
        expected: Option<Revision>,
        plan: PutPlanner,
    ) -> Result<PutResult> {
        let _lock = lock_repo(&self.root)?;
        let key = record.key.clone();
        let current = self.read(&key)?;
        // The decision (recreation re-stamping for `put`, verbatim for
        // `put_raw`, ancestor folding for both) is the shared core planner's
        // (spec §3.2).
        match plan(current.as_ref(), record, expected, self.cap) {
            PutPlan::Write(stored) => {
                self.write_and_commit(&stored, &format!("put {key}"))?;
                Ok(PutResult::Committed(stored.revision))
            }
            PutPlan::Conflict(conflict) => Ok(PutResult::Conflict(conflict)),
            PutPlan::NotFound => Err(CoreError::NotFound(key)),
            // Only consumer `plan_put` produces this, for a
            // `RecordKind::Tombstone` record: deletes go through `delete_as`,
            // replication through `put_raw`.
            PutPlan::Rejected(reason) => Err(CoreError::Backend(reason.to_string())),
        }
    }

    /// Write `record` to its worktree file and commit it with `message`.
    /// Call only while holding `lock_repo`.
    fn write_and_commit(&self, record: &Record, message: &str) -> Result<()> {
        let rel = rel_path(&record.key);
        let abs = self.root.join(&rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(be)?;
        }
        let bytes =
            serde_json::to_vec_pretty(record).map_err(|e| CoreError::Serde(e.to_string()))?;
        std::fs::write(&abs, &bytes).map_err(be)?;
        self.commit_file(&rel, message)
    }

    /// Remove `key`'s worktree file and commit the removal with `message`.
    /// Call only while holding `lock_repo`.
    fn remove_and_commit(&self, key: &RecordKey, message: &str) -> Result<()> {
        let rel = rel_path(key);
        match std::fs::remove_file(self.root.join(&rel)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(be(e)),
        }
        self.commit_removal(&rel, message)
    }

    /// Whether consumer `list` reports `key`. A tombstone is hidden. A file
    /// that vanished since the directory walk (a concurrent `purge`) is
    /// dropped. A file that fails to read for any other reason (fails to
    /// deserialize, or an unreadable entry such as a stray directory named
    /// `*.json`) stays listed, exactly as before tombstones, so `get` keeps
    /// surfacing the error instead of the key silently disappearing.
    fn is_listed(&self, key: &RecordKey) -> bool {
        match self.read(key) {
            Ok(Some(rec)) => !rec.is_tombstone(),
            Ok(None) => false,
            Err(_) => true,
        }
    }

    fn path_for(&self, key: &RecordKey) -> PathBuf {
        let (ns, col, file) = record_components(key);
        self.root.join(ns).join(col).join(file)
    }

    fn read(&self, key: &RecordKey) -> Result<Option<Record>> {
        let path = self.path_for(key);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).map_err(|e| CoreError::Serde(e.to_string()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CoreError::Backend(e.to_string())),
        }
    }

    fn commit_file(&self, rel: &Path, message: &str) -> Result<()> {
        let repo =
            git2::Repository::open(&self.root).map_err(|e| CoreError::Backend(e.to_string()))?;
        let mut index = repo
            .index()
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        index
            .add_path(rel)
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        index
            .write()
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        let tree_oid = index
            .write_tree()
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        let tree = repo
            .find_tree(tree_oid)
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        let sig = git2::Signature::now("gonzalo", "gonzalo@localhost")
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        let parent = repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .and_then(|oid| repo.find_commit(oid).ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        Ok(())
    }

    /// Commit the removal of `rel` (a `git rm` equivalent): stage the deletion in
    /// the index, write the tree, and record a commit. Mirrors `commit_file` but
    /// removes the path from the index instead of adding it.
    fn commit_removal(&self, rel: &Path, message: &str) -> Result<()> {
        let repo =
            git2::Repository::open(&self.root).map_err(|e| CoreError::Backend(e.to_string()))?;
        let mut index = repo
            .index()
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        index
            .remove_path(rel)
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        index
            .write()
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        let tree_oid = index
            .write_tree()
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        let tree = repo
            .find_tree(tree_oid)
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        let sig = git2::Signature::now("gonzalo", "gonzalo@localhost")
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        let parent = repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .and_then(|oid| repo.find_commit(oid).ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        Ok(())
    }

    /// Pull `branch` from `remote` (typically "origin"). A fast-forward advances
    /// the branch; a divergence is reconciled by a content-aware 3-way merge
    /// (gonzalo `merge()` per record, ADR 0017), with tombstones decided by
    /// kind and revision first (spec §3.5), and unresolved records kept local
    /// and reported in the [`PullReport`].
    pub async fn pull(&self, remote: &str, branch: &str) -> Result<PullReport> {
        let root = self.root.clone();
        let remote = remote.to_string();
        let branch = branch.to_string();
        let cap = self.cap;
        run_blocking(move || git_pull(&root, &remote, &branch, cap)).await
    }

    /// Push `branch` to `remote`.
    pub async fn push(&self, remote: &str, branch: &str) -> Result<()> {
        let root = self.root.clone();
        let remote = remote.to_string();
        let branch = branch.to_string();
        run_blocking(move || git_push(&root, &remote, &branch)).await
    }
}

/// The repo-relative path of `key`'s record file: `<ns>/<col>/<id>.json`.
fn rel_path(key: &RecordKey) -> PathBuf {
    let (ns, col, file) = record_components(key);
    Path::new(&ns).join(&col).join(&file)
}

fn be<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Backend(e.to_string())
}

/// Acquire the repo-level exclusive lock guarding the OCC critical section of
/// `put`, `delete` and `purge`.
///
/// Unlike `FsStore`, whose per-record lock suffices, every `GitStore` write
/// mutates the *shared* on-disk index and HEAD (via `commit_file` /
/// `commit_removal`), so serialization must be repo-wide: two writes on
/// different keys still race on the same index+HEAD. The lock is a
/// `<root>/.gonzalo-git.lock` file held exclusively via `flock`; it is released
/// when the returned handle drops, which covers every exit path (the
/// `Conflict`/`NotFound`/no-op early returns and any error). Blocking by
/// design — call only from the `spawn_blocking` section.
fn lock_repo(root: &Path) -> Result<std::fs::File> {
    let lock_path = root.join(".gonzalo-git.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(be)?;
    flock(&lock, FlockOperation::LockExclusive).map_err(be)?;
    Ok(lock)
}

fn git_pull(root: &Path, remote: &str, branch: &str, cap: usize) -> Result<PullReport> {
    // Hold the repo lock that `put`, `delete` and `purge` take, so a concurrent
    // local write can't commit mid-pull and then be overwritten by the forced
    // checkout below (spec §3.5). Nothing below re-takes it.
    let _lock = lock_repo(root)?;
    let repo = git2::Repository::open(root).map_err(be)?;
    let mut rem = repo.find_remote(remote).map_err(be)?;
    rem.fetch(&[branch], None, None).map_err(be)?;
    let fetch_head = repo.find_reference("FETCH_HEAD").map_err(be)?;
    let fetch_commit = repo
        .reference_to_annotated_commit(&fetch_head)
        .map_err(be)?;
    let (analysis, _) = repo.merge_analysis(&[&fetch_commit]).map_err(be)?;

    if analysis.is_up_to_date() {
        return Ok(PullReport::default());
    }
    if analysis.is_fast_forward() {
        let refname = format!("refs/heads/{branch}");
        let mut reference = repo.find_reference(&refname).map_err(be)?;
        reference
            .set_target(fetch_commit.id(), "fast-forward")
            .map_err(be)?;
        repo.set_head(&refname).map_err(be)?;
        repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))
            .map_err(be)?;
        return Ok(PullReport {
            fast_forwarded: true,
            ..Default::default()
        });
    }

    merge_non_ff(&repo, remote, branch, fetch_commit.id(), cap)
}

/// Reconcile a diverged local branch with `remote_oid` by a content-aware 3-way
/// merge, recorded in a two-parent merge commit (ADR 0017).
///
/// A path changed only on the remote takes the remote side verbatim. That
/// includes a remote tombstone (a modified file) and a remote purge (a git
/// deletion). A record changed on both sides is decided by kind before any body
/// merge (spec §3.5): equal revisions are a no-op; two tombstones converge on
/// the higher `(counter, hash)`; exactly one tombstone is a `PullConflict` that
/// keeps local; two live records take gonzalo's class-aware `merge()`; a local
/// purge against a remote edit takes the remote record. Records written into
/// the index bypass `put_raw`, so ancestors are truncated here to `cap`.
fn merge_non_ff(
    repo: &git2::Repository,
    remote: &str,
    branch: &str,
    remote_oid: git2::Oid,
    cap: usize,
) -> Result<PullReport> {
    let local_oid = repo
        .head()
        .map_err(be)?
        .target()
        .ok_or_else(|| CoreError::Backend("local HEAD is unborn".into()))?;
    let local_commit = repo.find_commit(local_oid).map_err(be)?;
    let remote_commit = repo.find_commit(remote_oid).map_err(be)?;
    let local_tree = local_commit.tree().map_err(be)?;
    let remote_tree = remote_commit.tree().map_err(be)?;
    // The merge base is the true common ancestor (git retains history); an
    // unrelated history has no base, so treat every overlap as add/add.
    let base_tree = match repo.merge_base(local_oid, remote_oid) {
        Ok(base_oid) => Some(repo.find_commit(base_oid).map_err(be)?.tree().map_err(be)?),
        Err(_) => None,
    };

    // Start the merged index from local, then fold in the remote-side changes.
    let mut index = repo.index().map_err(be)?;
    index.read_tree(&local_tree).map_err(be)?;

    let local_changed = changed_paths_set(repo, base_tree.as_ref(), &local_tree)?;
    let remote_diff = repo
        .diff_tree_to_tree(base_tree.as_ref(), Some(&remote_tree), None)
        .map_err(be)?;

    let mut report = PullReport::default();
    for delta in remote_diff.deltas() {
        let Some(path) = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(Path::to_path_buf)
        else {
            continue;
        };

        if !local_changed.contains(&path) {
            // Changed only on the remote: apply the remote side verbatim. A
            // remote tombstone is a modified file; a remote purge is a deletion.
            if delta.status() == git2::Delta::Deleted {
                index.remove_path(&path).map_err(be)?;
            } else if let Some(bytes) = tree_blob(repo, &remote_tree, &path)? {
                index
                    .add_frombuffer(&blob_entry(&path), &bytes)
                    .map_err(be)?;
            }
            continue;
        }

        // Changed on both sides.
        let Some(key) = key_from_path(&path) else {
            continue; // non-record file (should not occur in a record store)
        };
        let local_rec = record_at(repo, Some(&local_tree), &path)?;
        let remote_rec = record_at(repo, Some(&remote_tree), &path)?;
        match (local_rec, remote_rec) {
            // Same revision, e.g. two independent deletes of one revision whose
            // files differ only in `deleted_at`: already in sync, keep local.
            (Some(local), Some(remote)) if local.revision == remote.revision => {}
            // Two diverged tombstones: the higher (counter, hash) wins, carrying
            // both chains. Checked by kind, because two tombstones always have
            // equal (empty) bodies and the body guard below would skip them.
            (Some(local), Some(remote)) if local.is_tombstone() && remote.is_tombstone() => {
                let winner = tombstone_winner(&local, &remote, cap);
                stage_record(&mut index, &path, &winner)?;
                report.merged.push(key);
            }
            // Delete vs edit: keep local (already staged) and surface both,
            // the same policy as an unmergeable body.
            (Some(local), Some(remote)) if local.is_tombstone() || remote.is_tombstone() => {
                report.conflicts.push(PullConflict {
                    key,
                    local: Box::new(local),
                    remote: Box::new(remote),
                });
            }
            (Some(local), Some(remote)) if local.body != remote.body => {
                let base_body = record_at(repo, base_tree.as_ref(), &path)?
                    .map(|r| r.body)
                    .unwrap_or(Body::Inline(Vec::new()));
                match merge(
                    local.kind.merge_class(),
                    &base_body,
                    &local.body,
                    &remote.body,
                ) {
                    MergeOutcome::Merged(body) => {
                        let merged = merged_record(&local, &remote, body, cap);
                        stage_record(&mut index, &path, &merged)?;
                        report.merged.push(key);
                    }
                    MergeOutcome::NeedsResolution => {
                        // Keep local (already staged); surface both sides.
                        report.conflicts.push(PullConflict {
                            key,
                            local: Box::new(local),
                            remote: Box::new(remote),
                        });
                    }
                }
            }
            // Local present, remote purged: keep local, which is already
            // staged from `local_tree` (sync's `(Some, None)` copy row).
            (Some(_local), None) => {}
            // Local purged, remote edited: take the remote record, so pull
            // never silently drops a concurrent remote edit (sync's
            // `(None, Some)` copy row).
            (None, Some(remote)) => {
                stage_record(&mut index, &path, &remote)?;
            }
            // Equal bodies, or absent on both sides: keep local.
            _ => {}
        }
    }

    // Commit the reconciled tree with both parents, then advance the branch.
    let tree_oid = index.write_tree_to(repo).map_err(be)?;
    let tree = repo.find_tree(tree_oid).map_err(be)?;
    // Check out the merged tree while HEAD still names the local commit, so
    // libgit2's baseline is the local tree and a path the merge removed (a
    // remote purge) is deleted from the worktree. Checking out after `set_head`
    // would see baseline == target and leave the file behind as untracked.
    // This also writes the merged index to disk. The untracked
    // `.gonzalo-git.lock` is neither in the baseline nor the target, so it stays.
    repo.checkout_tree(
        tree.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .map_err(be)?;
    let sig = git2::Signature::now("gonzalo", "gonzalo@localhost").map_err(be)?;
    let refname = format!("refs/heads/{branch}");
    repo.commit(
        Some(&refname),
        &sig,
        &sig,
        &format!("merge {remote}/{branch}"),
        &tree,
        &[&local_commit, &remote_commit],
    )
    .map_err(be)?;
    repo.set_head(&refname).map_err(be)?;
    // No trailing `checkout_head`: the worktree and index already match the
    // committed tree, so it would be a no-op.

    Ok(report)
}

/// Serialize `record` as the store does on `put` and stage it at `path`.
fn stage_record(index: &mut git2::Index, path: &Path, record: &Record) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(record).map_err(|e| CoreError::Serde(e.to_string()))?;
    index.add_frombuffer(&blob_entry(path), &bytes).map_err(be)
}

/// Paths that differ between `base` (an empty tree if `None`) and `tree`.
fn changed_paths_set(
    repo: &git2::Repository,
    base: Option<&git2::Tree>,
    tree: &git2::Tree,
) -> Result<HashSet<PathBuf>> {
    let diff = repo.diff_tree_to_tree(base, Some(tree), None).map_err(be)?;
    let mut set = HashSet::new();
    for delta in diff.deltas() {
        if let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path()) {
            set.insert(path.to_path_buf());
        }
    }
    Ok(set)
}

/// The `Record` stored at `path` in `tree` (`None` if `tree` is `None` or the
/// path is absent).
fn record_at(
    repo: &git2::Repository,
    tree: Option<&git2::Tree>,
    path: &Path,
) -> Result<Option<Record>> {
    let Some(tree) = tree else {
        return Ok(None);
    };
    match tree.get_path(path) {
        Ok(entry) => {
            let obj = entry.to_object(repo).map_err(be)?;
            let blob = obj
                .as_blob()
                .ok_or_else(|| CoreError::Backend("record path is not a blob".into()))?;
            let rec = serde_json::from_slice(blob.content())
                .map_err(|e| CoreError::Serde(e.to_string()))?;
            Ok(Some(rec))
        }
        Err(_) => Ok(None),
    }
}

/// Raw blob bytes at `path` in `tree`, or `None` if absent.
fn tree_blob(repo: &git2::Repository, tree: &git2::Tree, path: &Path) -> Result<Option<Vec<u8>>> {
    match tree.get_path(path) {
        Ok(entry) => {
            let obj = entry.to_object(repo).map_err(be)?;
            Ok(obj.as_blob().map(|b| b.content().to_vec()))
        }
        Err(_) => Ok(None),
    }
}

/// The `RecordKey` for a `ns/col/id.json` path, or `None` if it isn't one.
fn key_from_path(path: &Path) -> Option<RecordKey> {
    let comps: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if comps.len() != 3 {
        return None;
    }
    let id = comps[2].strip_suffix(".json")?;
    Some(RecordKey::new(
        decode_segment(&comps[0]),
        decode_segment(&comps[1]),
        decode_segment(id),
    ))
}

/// A blank index entry for a regular file `path`; `add_frombuffer` fills the oid
/// from the data.
fn blob_entry(path: &Path) -> git2::IndexEntry {
    git2::IndexEntry {
        ctime: git2::IndexTime::new(0, 0),
        mtime: git2::IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: 0o100644,
        uid: 0,
        gid: 0,
        file_size: 0,
        id: git2::Oid::zero(),
        flags: 0,
        flags_extended: 0,
        path: path.to_string_lossy().into_owned().into_bytes(),
    }
}

/// The merged record from an auto-resolved divergence: the shared core
/// reconciliation (the same construction as `sync`), authored as
/// `gonzalo-merge` from `git-pull`, with ancestors truncated to `cap`.
fn merged_record(local: &Record, remote: &Record, body: Body, cap: usize) -> Record {
    reconciled_record(
        local,
        remote,
        body,
        Identity::new("gonzalo-merge"),
        "git-pull",
        cap,
    )
}

fn git_push(root: &Path, remote: &str, branch: &str) -> Result<()> {
    let repo = git2::Repository::open(root).map_err(be)?;
    let mut rem = repo.find_remote(remote).map_err(be)?;
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");

    // libgit2's `push` returns Ok even when the remote refuses a ref update
    // (e.g. non-fast-forward): the per-ref verdict arrives ONLY through the
    // `push_update_reference` callback, whose `status` is `Some(msg)` on
    // rejection and `None` on success. Capture every rejection so we can fail
    // the push instead of silently reporting success.
    let rejected: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut callbacks = git2::RemoteCallbacks::new();
    {
        let rejected = Rc::clone(&rejected);
        callbacks.push_update_reference(move |refname, status| {
            if let Some(msg) = status {
                rejected.borrow_mut().push(format!("{refname}: {msg}"));
            }
            Ok(())
        });
    }
    let mut opts = git2::PushOptions::new();
    opts.remote_callbacks(callbacks);
    rem.push(&[refspec.as_str()], Some(&mut opts)).map_err(be)?;
    drop(opts); // release the callback's borrow of `rejected` before we read it

    let rejected = rejected.borrow();
    if !rejected.is_empty() {
        return Err(CoreError::Backend(format!(
            "push rejected by remote '{remote}': {}",
            rejected.join(", ")
        )));
    }
    Ok(())
}

async fn run_blocking<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| CoreError::Backend(e.to_string()))?
}

#[async_trait]
impl gonzalo_core::Store for GitStore {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        // Consumer read: a tombstoned key looks absent (spec §3.2).
        let store = self.handle();
        let key = key.clone();
        run_blocking(move || Ok(store.read(&key)?.filter(|rec| !rec.is_tombstone()))).await
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        let store = self.handle();
        run_blocking(move || store.put_locked(record, expected, plan_put)).await
    }

    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // Replication write: same repo-wide critical section as `put`, decided
        // by `plan_put_raw`, which never re-stamps. A create over a tombstone is
        // a Conflict, never a recreation.
        let store = self.handle();
        run_blocking(move || store.put_locked(record, expected, plan_put_raw)).await
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        // Consumer listing excludes tombstoned keys, which means reading each
        // record file under the prefix (spec §8.4: a local read per key).
        let store = self.handle();
        let prefix = prefix.clone();
        run_blocking(move || {
            let mut keys = Vec::new();
            collect_keys(&store.root, &prefix, &mut keys)?;
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                if store.is_listed(&key) {
                    out.push(key);
                }
            }
            Ok(out)
        })
        .await
    }

    // No `delete` here: the trait provides it as `delete_as(key, expected, None)`.
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult> {
        let store = self.handle();
        let key = key.clone();
        run_blocking(move || {
            // Serialize the read→plan→write→commit critical section over the
            // shared index+HEAD, exactly as `put`; the lock releases when `_lock`
            // drops (all paths).
            let _lock = lock_repo(&store.root)?;
            let current = store.read(&key)?;
            // A delete commits a tombstone at the record's normal path, so git
            // history shows it as an ordinary modification (spec §3.3, §5.5).
            // A no-op (absent key, or already a tombstone) makes no commit.
            // `Some(author)` replaces `meta.author` on the tombstone.
            match plan_delete(
                current.as_ref(),
                expected,
                now_ms(),
                store.cap,
                author.as_ref(),
            ) {
                DeletePlan::Write(tombstone) => {
                    store.write_and_commit(&tombstone, &format!("delete {key}"))?;
                    Ok(DeleteResult::Deleted)
                }
                DeletePlan::Noop => Ok(DeleteResult::Deleted),
                DeletePlan::Conflict(conflict) => Ok(DeleteResult::Conflict(conflict)),
            }
        })
        .await
    }

    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        // Replication read: whatever the worktree holds, tombstones included.
        let store = self.handle();
        let key = key.clone();
        run_blocking(move || store.read(&key)).await
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        // Replication listing: every `<id>.json` under the prefix, tombstones
        // included, without reading any file.
        let store = self.handle();
        let prefix = prefix.clone();
        run_blocking(move || {
            let mut out = Vec::new();
            collect_keys(&store.root, &prefix, &mut out)?;
            Ok(out)
        })
        .await
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        let store = self.handle();
        let key = key.clone();
        run_blocking(move || {
            // The only physical removal in the system, in the same repo-wide
            // critical section as `put`: read, decide (`plan_purge`), then
            // remove the file and commit the removal.
            let _lock = lock_repo(&store.root)?;
            let current = store.read(&key)?;
            match plan_purge(current.as_ref(), &expected) {
                PurgePlan::Remove => {
                    store.remove_and_commit(&key, &format!("purge {key}"))?;
                    Ok(DeleteResult::Deleted)
                }
                PurgePlan::Noop => Ok(DeleteResult::Deleted),
                PurgePlan::Conflict(conflict) => Ok(DeleteResult::Conflict(conflict)),
            }
        })
        .await
    }
}

fn collect_keys(root: &Path, prefix: &KeyPrefix, out: &mut Vec<RecordKey>) -> Result<()> {
    let namespaces = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CoreError::Backend(e.to_string())),
    };
    for ns in namespaces {
        let ns = ns.map_err(|e| CoreError::Backend(e.to_string()))?;
        let ns_name = ns.file_name().to_string_lossy().to_string();
        if ns_name == ".git" || !ns.path().is_dir() {
            continue;
        }
        for col in std::fs::read_dir(ns.path()).map_err(|e| CoreError::Backend(e.to_string()))? {
            let col = col.map_err(|e| CoreError::Backend(e.to_string()))?;
            if !col.path().is_dir() {
                continue;
            }
            let col_name = col.file_name().to_string_lossy().to_string();
            for f in std::fs::read_dir(col.path()).map_err(|e| CoreError::Backend(e.to_string()))? {
                let f = f.map_err(|e| CoreError::Backend(e.to_string()))?;
                let fname = f.file_name().to_string_lossy().to_string();
                if let Some(id) = fname.strip_suffix(".json") {
                    // Path components are `segment`-encoded; decode to recover
                    // the original key so `list()` round-trips.
                    let key = RecordKey::new(
                        decode_segment(&ns_name),
                        decode_segment(&col_name),
                        decode_segment(id),
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
