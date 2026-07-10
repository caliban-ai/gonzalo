//! Git-backed storage substrate. Each record is a JSON file in a git
//! worktree; every write is committed, giving an auditable history. Remote
//! replication via fast-forward `pull`/`push`.

use async_trait::async_trait;
use gonzalo_core::{
    Body, ContentHash, CoreError, Identity, KeyPrefix, MergeOutcome, Meta, PutResult, Record,
    RecordKey, Result, Revision, decode_segment, merge, record_components, store::Conflict,
};
use rustix::fs::{FlockOperation, flock};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

mod diff;
pub use diff::{ChangedPaths, changed_paths, head_commit, is_git_repo};

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
        Ok(Self { root })
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

    /// Pull `branch` from `remote` (typically "origin"). A fast-forward advances
    /// the branch; a divergence is reconciled by a content-aware 3-way merge
    /// (gonzalo `merge()` per record, ADR 0017), with unresolved records kept
    /// local and reported in the [`PullReport`].
    pub async fn pull(&self, remote: &str, branch: &str) -> Result<PullReport> {
        let root = self.root.clone();
        let remote = remote.to_string();
        let branch = branch.to_string();
        run_blocking(move || git_pull(&root, &remote, &branch)).await
    }

    /// Push `branch` to `remote`.
    pub async fn push(&self, remote: &str, branch: &str) -> Result<()> {
        let root = self.root.clone();
        let remote = remote.to_string();
        let branch = branch.to_string();
        run_blocking(move || git_push(&root, &remote, &branch)).await
    }
}

fn be<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Backend(e.to_string())
}

/// Acquire the repo-level exclusive lock guarding `put`'s OCC critical section.
///
/// Unlike `FsStore`, whose per-record lock suffices, `GitStore::put` mutates the
/// *shared* on-disk index and HEAD (via `commit_file`), so serialization must be
/// repo-wide: two puts on different keys still race on the same index+HEAD. The
/// lock is a `<root>/.gonzalo-git.lock` file held exclusively via `flock`; it is
/// released when the returned handle drops, which covers every `put` exit path
/// (the `Conflict`/`NotFound` early returns and any error). Blocking by design —
/// call only from the `spawn_blocking` section.
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

fn git_pull(root: &Path, remote: &str, branch: &str) -> Result<PullReport> {
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

    merge_non_ff(&repo, remote, branch, fetch_commit.id())
}

/// Reconcile a diverged local branch with `remote_oid` by a content-aware 3-way
/// merge: each record changed on both sides is merged with gonzalo's
/// class-aware `merge()`, and the result is recorded in a two-parent merge
/// commit (ADR 0017).
fn merge_non_ff(
    repo: &git2::Repository,
    remote: &str,
    branch: &str,
    remote_oid: git2::Oid,
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
            // Changed only on the remote — apply the remote side verbatim.
            if delta.status() == git2::Delta::Deleted {
                index.remove_path(&path).map_err(be)?;
            } else if let Some(bytes) = tree_blob(repo, &remote_tree, &path)? {
                index
                    .add_frombuffer(&blob_entry(&path), &bytes)
                    .map_err(be)?;
            }
            continue;
        }

        // Changed on both sides — reconcile with gonzalo's class-aware merge.
        let Some(key) = key_from_path(&path) else {
            continue; // non-record file (should not occur in a record store)
        };
        let local_rec = record_at(repo, Some(&local_tree), &path)?;
        let remote_rec = record_at(repo, Some(&remote_tree), &path)?;
        match (local_rec, remote_rec) {
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
                        let merged = merged_record(&key, &local, &remote, body);
                        let bytes = serde_json::to_vec_pretty(&merged)
                            .map_err(|e| CoreError::Serde(e.to_string()))?;
                        index
                            .add_frombuffer(&blob_entry(&path), &bytes)
                            .map_err(be)?;
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
            // One-sided presence (modify/delete, or identical edits): keep the
            // local side, which is already staged from `local_tree`.
            _ => {}
        }
    }

    // Commit the reconciled tree with both parents, then advance the branch.
    let tree_oid = index.write_tree_to(repo).map_err(be)?;
    let tree = repo.find_tree(tree_oid).map_err(be)?;
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
    repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))
        .map_err(be)?;

    Ok(report)
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

/// The merged record from an auto-resolved divergence: a fresh revision over the
/// merged `body`, mirroring `sync`'s merged-record construction.
fn merged_record(key: &RecordKey, local: &Record, remote: &Record, body: Body) -> Record {
    let counter = local.revision.counter.max(remote.revision.counter) + 1;
    let mut labels = local.meta.labels.clone();
    labels.extend(remote.meta.labels.clone());
    let mut links = local.links.clone();
    for l in &remote.links {
        if !links.contains(l) {
            links.push(l.clone());
        }
    }
    let parent = if local.revision.counter >= remote.revision.counter {
        local.revision.clone()
    } else {
        remote.revision.clone()
    };
    Record {
        key: key.clone(),
        kind: local.kind,
        revision: Revision {
            counter,
            hash: ContentHash::of(body.bytes()),
        },
        parent: Some(parent),
        body,
        meta: Meta {
            author: Identity::new("gonzalo-merge"),
            origin_system: "git-pull".into(),
            created: local.meta.created.min(remote.meta.created),
            updated: local.meta.updated.max(remote.meta.updated),
            labels,
        },
        links,
    }
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
        let this = Arc::new(self.root.clone());
        let key = key.clone();
        run_blocking(move || {
            let store = GitStore {
                root: (*this).clone(),
            };
            store.read(&key)
        })
        .await
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        let root = self.root.clone();
        run_blocking(move || {
            let store = GitStore { root: root.clone() };
            // Serialize the read→check→write→commit critical section over the
            // shared index+HEAD; the lock releases when `_lock` drops (all paths).
            let _lock = lock_repo(&root)?;
            let current = store.read(&record.key)?;
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
            let (ns, col, file) = record_components(&record.key);
            let rel = Path::new(&ns).join(&col).join(&file);
            let abs = root.join(&rel);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).map_err(|e| CoreError::Backend(e.to_string()))?;
            }
            let bytes =
                serde_json::to_vec_pretty(&record).map_err(|e| CoreError::Serde(e.to_string()))?;
            std::fs::write(&abs, &bytes).map_err(|e| CoreError::Backend(e.to_string()))?;
            store.commit_file(&rel, &format!("put {}", record.key))?;
            Ok(PutResult::Committed(record.revision))
        })
        .await
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        let root = self.root.clone();
        let prefix = prefix.clone();
        run_blocking(move || {
            let mut out = Vec::new();
            collect_keys(&root, &prefix, &mut out)?;
            Ok(out)
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
