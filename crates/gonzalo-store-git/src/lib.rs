//! Git-backed storage substrate. Each record is a JSON file in a git
//! worktree; every write is committed, giving an auditable history. Remote
//! replication via fast-forward `pull`/`push`.

use async_trait::async_trait;
use gonzalo_core::{
    CoreError, KeyPrefix, PutResult, Record, RecordKey, Result, Revision, record_components,
    store::Conflict,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
        let repo = git2::Repository::open(&self.root)
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        let mut index = repo.index().map_err(|e| CoreError::Backend(e.to_string()))?;
        index.add_path(rel).map_err(|e| CoreError::Backend(e.to_string()))?;
        index.write().map_err(|e| CoreError::Backend(e.to_string()))?;
        let tree_oid = index.write_tree().map_err(|e| CoreError::Backend(e.to_string()))?;
        let tree = repo.find_tree(tree_oid).map_err(|e| CoreError::Backend(e.to_string()))?;
        let sig = git2::Signature::now("gonzalo", "gonzalo@localhost")
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        let parent = repo.head().ok().and_then(|h| h.target()).and_then(|oid| repo.find_commit(oid).ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        Ok(())
    }

    /// Fast-forward pull from `remote` (default "origin"). Errors if the
    /// local branch cannot be fast-forwarded.
    pub async fn pull(&self, remote: &str, branch: &str) -> Result<()> {
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

fn git_pull(root: &Path, remote: &str, branch: &str) -> Result<()> {
    let repo = git2::Repository::open(root).map_err(|e| CoreError::Backend(e.to_string()))?;
    let mut rem = repo.find_remote(remote).map_err(|e| CoreError::Backend(e.to_string()))?;
    rem.fetch(&[branch], None, None).map_err(|e| CoreError::Backend(e.to_string()))?;
    let fetch_head = repo
        .find_reference("FETCH_HEAD")
        .map_err(|e| CoreError::Backend(e.to_string()))?;
    let fetch_commit = repo
        .reference_to_annotated_commit(&fetch_head)
        .map_err(|e| CoreError::Backend(e.to_string()))?;
    let (analysis, _) = repo.merge_analysis(&[&fetch_commit]).map_err(|e| CoreError::Backend(e.to_string()))?;
    if analysis.is_up_to_date() {
        Ok(())
    } else if analysis.is_fast_forward() {
        let refname = format!("refs/heads/{branch}");
        let mut reference = repo
            .find_reference(&refname)
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        reference
            .set_target(fetch_commit.id(), "fast-forward")
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        repo.set_head(&refname).map_err(|e| CoreError::Backend(e.to_string()))?;
        repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        Ok(())
    } else {
        Err(CoreError::Backend("non-fast-forward pull requires manual merge".into()))
    }
}

fn git_push(root: &Path, remote: &str, branch: &str) -> Result<()> {
    let repo = git2::Repository::open(root).map_err(|e| CoreError::Backend(e.to_string()))?;
    let mut rem = repo.find_remote(remote).map_err(|e| CoreError::Backend(e.to_string()))?;
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    rem.push(&[refspec.as_str()], None).map_err(|e| CoreError::Backend(e.to_string()))?;
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
            let store = GitStore { root: (*this).clone() };
            store.read(&key)
        })
        .await
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        let root = self.root.clone();
        run_blocking(move || {
            let store = GitStore { root: root.clone() };
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
