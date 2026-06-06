# Gonzalo M2 — Git + S3 Substrates + Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Add two more storage substrates (`gonzalo-store-git`, `gonzalo-store-s3`) behind the existing `Store` trait, and a `Sync` engine in `gonzalo-core` that reconciles any two `Store`s using the existing merge strategies — fulfilling design spec §3 (substrates), §6 (sync).

**Architecture:** All substrates implement the same `gonzalo_core::Store`. Path/key mapping is shared via a new `gonzalo_core::paths` module (DRY across fs/git/s3). `Sync` is pure logic over the `Store` trait (no concrete backend), reusing `merge()` for append-only auto-merge and surfacing everything else as conflicts in a `SyncReport`.

**Tech Stack:** `git2` (libgit2) for git; `aws-sdk-s3` + `aws-config` (rustls) for S3-compatible object storage; existing `tokio`, `serde_json`, `blake3`.

**Design decisions (accepted recommendations):**
- Git substrate commits on every write (auditable history); `pull`/`push` against a git remote are fast-forward-only in M2 (non-FF surfaces an error for manual resolution).
- S3 OCC is read-current-then-conditional-write (TOCTOU window, same as fs — documented; native conditional PUT via ETag deferred).
- Sync without stored ancestry: AppendOnly kinds auto-merge by union (empty-base merge yields the union, which is correct for append-only); Structured/Opaque divergences are surfaced as conflicts, not auto-resolved.

---

## Task 1: `gonzalo-core` — shared `paths` module + refactor FsStore

**Files:**
- Create: `crates/gonzalo-core/src/paths.rs`
- Modify: `crates/gonzalo-core/src/lib.rs`
- Modify: `crates/gonzalo-store-fs/src/layout.rs` (use the shared helper)

- [ ] **Step 1: Write `crates/gonzalo-core/src/paths.rs`**

```rust
//! Shared, filesystem/object-key path mapping used by every storage
//! substrate so a record lands at the same logical location regardless of
//! backend.

use crate::RecordKey;

/// Encode one key component as a single safe path/key segment. Only
/// `[A-Za-z0-9_-]` survive; everything else (including `.`, `/`, and dot
/// lookalikes) maps to `_`, so `..` and path separators cannot escape.
pub fn segment(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

/// The three sanitized path components for a record:
/// `(namespace_dir, collection_dir, "<id>.json")`. Backends join these with
/// their own separator (`PathBuf` for fs/git, `/` for object keys).
pub fn record_components(key: &RecordKey) -> (String, String, String) {
    (
        segment(&key.namespace),
        segment(&key.collection),
        format!("{}.json", segment(&key.id)),
    )
}

/// The object-key form `namespace/collection/id.json` for object stores.
pub fn object_key(key: &RecordKey) -> String {
    let (ns, col, file) = record_components(key);
    format!("{ns}/{col}/{file}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_neutralizes_traversal() {
        assert_eq!(segment(".."), "__");
        assert_eq!(segment("../etc"), "___etc");
        assert_eq!(segment("a.b"), "a_b");
    }

    #[test]
    fn object_key_is_slash_joined_json() {
        let k = RecordKey::new("caliban", "topics", "rust");
        assert_eq!(object_key(&k), "caliban/topics/rust.json");
    }
}
```

- [ ] **Step 2: Wire module + re-exports in `crates/gonzalo-core/src/lib.rs`**

Add:
```rust
pub mod paths;
pub use paths::{object_key, record_components, segment};
```

- [ ] **Step 3: Refactor `crates/gonzalo-store-fs/src/layout.rs` to use the shared helper**

Replace the local `seg` function and `record_path` body so `record_path` is:
```rust
use gonzalo_core::{RecordKey, record_components};
use std::path::{Path, PathBuf};

/// The file path for a record's JSON under `root`.
pub fn record_path(root: &Path, key: &RecordKey) -> PathBuf {
    let (ns, col, file) = record_components(key);
    root.join(ns).join(col).join(file)
}
```
Keep the existing `#[cfg(test)] mod tests` in `layout.rs` (both tests must still pass — `record_path_is_nested_json` and `unsafe_chars_are_neutralized`). Remove the now-unused local `seg` fn.

- [ ] **Step 4: Verify**

Run: `cargo test -p gonzalo-core -p gonzalo-store-fs` — all pass (new paths tests + existing layout/store tests).
Run: `cargo clippy -p gonzalo-core -p gonzalo-store-fs --all-targets --features gonzalo-core/conformance -- -D warnings` — clean.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "$(printf 'refactor(core): add shared paths module, dedupe fs layout\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 2: `gonzalo-core` — Sync engine

**Files:**
- Create: `crates/gonzalo-core/src/sync.rs`
- Modify: `crates/gonzalo-core/src/lib.rs`

- [ ] **Step 1: Write `crates/gonzalo-core/src/sync.rs`**

```rust
//! Reconcile two `Store`s. Any store can be a sync peer. Append-only kinds
//! auto-merge by union; structured/opaque divergences are surfaced as
//! conflicts. No stored ancestry yet (M2): the merge uses an empty base,
//! which is correct for append-only union.

use crate::{
    Body, Identity, KeyPrefix, MergeOutcome, Meta, Record, RecordKey, Result, Revision, Store,
    merge, store::PutResult,
};
use std::collections::BTreeSet;

/// A divergence that could not be auto-merged and needs caller/CLI resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncConflict {
    pub key: RecordKey,
    pub a: Box<Record>,
    pub b: Box<Record>,
}

/// What a sync run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use = "a SyncReport may contain unresolved conflicts that must be handled"]
pub struct SyncReport {
    /// Keys copied into store A (were only in B).
    pub copied_to_a: Vec<RecordKey>,
    /// Keys copied into store B (were only in A).
    pub copied_to_b: Vec<RecordKey>,
    /// Keys auto-merged (append-only) and written to both stores.
    pub merged: Vec<RecordKey>,
    /// Divergences needing manual resolution.
    pub conflicts: Vec<SyncConflict>,
}

/// Reconcile stores `a` and `b`. After a clean run (no `conflicts`), both
/// stores hold the same set of records for every key.
pub async fn sync(a: &dyn Store, b: &dyn Store) -> Result<SyncReport> {
    let mut report = SyncReport::default();

    let mut keys: BTreeSet<RecordKey> = BTreeSet::new();
    keys.extend(a.list(&KeyPrefix::default()).await?);
    keys.extend(b.list(&KeyPrefix::default()).await?);

    for key in keys {
        let ra = a.get(&key).await?;
        let rb = b.get(&key).await?;
        match (ra, rb) {
            (Some(rec), None) => {
                copy(b, &rec).await?;
                report.copied_to_b.push(key);
            }
            (None, Some(rec)) => {
                copy(a, &rec).await?;
                report.copied_to_a.push(key);
            }
            (Some(rec_a), Some(rec_b)) => {
                if rec_a.revision == rec_b.revision {
                    continue; // already in sync
                }
                match merge(
                    rec_a.kind.merge_class(),
                    &Body::Inline(Vec::new()),
                    &rec_a.body,
                    &rec_b.body,
                ) {
                    MergeOutcome::Merged(body) => {
                        let merged = build_merged(&key, &rec_a, &rec_b, body);
                        overwrite(a, &merged, &rec_a.revision).await?;
                        overwrite(b, &merged, &rec_b.revision).await?;
                        report.merged.push(key);
                    }
                    MergeOutcome::NeedsResolution => {
                        report.conflicts.push(SyncConflict {
                            key,
                            a: Box::new(rec_a),
                            b: Box::new(rec_b),
                        });
                    }
                }
            }
            (None, None) => {}
        }
    }
    Ok(report)
}

async fn copy(dst: &dyn Store, rec: &Record) -> Result<()> {
    // Create in dst; if dst already changed concurrently we leave it (M2).
    let _ = dst.put(rec.clone(), None).await?;
    Ok(())
}

async fn overwrite(dst: &dyn Store, rec: &Record, expected: &Revision) -> Result<()> {
    let _ = dst.put(rec.clone(), Some(expected.clone())).await?;
    Ok(())
}

fn build_merged(key: &RecordKey, a: &Record, b: &Record, body: Body) -> Record {
    let counter = a.revision.counter.max(b.revision.counter) + 1;
    let mut labels = a.meta.labels.clone();
    labels.extend(b.meta.labels.clone());
    Record {
        key: key.clone(),
        kind: a.kind,
        revision: Revision { counter, hash: crate::ContentHash::of(body.bytes()) },
        parent: Some(if a.revision.counter >= b.revision.counter {
            a.revision.clone()
        } else {
            b.revision.clone()
        }),
        body,
        meta: Meta {
            author: Identity::new("gonzalo-sync"),
            origin_system: "sync".into(),
            created: a.meta.created.min(b.meta.created),
            updated: a.meta.updated.max(b.meta.updated),
            labels,
        },
        links: a.links.clone(),
    }
}
```

> NOTE: `overwrite` returns `Ok(())` even if the conditional put yields a
> `PutResult::Conflict` (concurrent mutation mid-sync). M2 assumes stores are
> quiescent during sync; tightening this to re-loop is deferred. The unused
> `PutResult` import is referenced by the doc above — if clippy flags it as
> unused, remove it from the `use` list.

- [ ] **Step 2: Wire module in `lib.rs`**

```rust
pub mod sync;
pub use sync::{SyncConflict, SyncReport, sync};
```

- [ ] **Step 3: Write tests** (`crates/gonzalo-core/src/sync.rs`, in a `#[cfg(test)] mod tests`)

Use `gonzalo_core`'s own conformance helpers indirectly is not possible (fs is downstream), so the sync tests run against an in-memory test store defined inline:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PutResult, RecordKind, store::Conflict};
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemStore(Mutex<BTreeMap<RecordKey, Record>>);

    #[async_trait]
    impl Store for MemStore {
        async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            let mut g = self.0.lock().unwrap();
            let current = g.get(&record.key).map(|r| r.revision.clone());
            if current != expected {
                if let Some(cur) = g.get(&record.key).cloned() {
                    return Ok(PutResult::Conflict(Box::new(Conflict {
                        key: record.key.clone(),
                        expected,
                        current: cur,
                    })));
                }
                return Err(crate::CoreError::NotFound(record.key.clone()));
            }
            let rev = record.revision.clone();
            g.insert(record.key.clone(), record);
            Ok(PutResult::Committed(rev))
        }
        async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .keys()
                .filter(|k| prefix.matches(k))
                .cloned()
                .collect())
        }
    }

    fn rec(id: &str, kind: RecordKind, payload: &str) -> Record {
        let body = Body::Inline(payload.as_bytes().to_vec());
        Record {
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            kind,
            key: RecordKey::new("ns", "col", id),
            meta: Meta {
                author: Identity::new("t"),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
        }
    }

    #[tokio::test]
    async fn copies_one_sided_records_both_directions() {
        let a = MemStore::default();
        let b = MemStore::default();
        let _ = a.put(rec("only_a", RecordKind::Topic, "x"), None).await.unwrap();
        let _ = b.put(rec("only_b", RecordKind::Topic, "y"), None).await.unwrap();

        let report = sync(&a, &b).await.unwrap();
        assert_eq!(report.copied_to_b, vec![RecordKey::new("ns", "col", "only_a")]);
        assert_eq!(report.copied_to_a, vec![RecordKey::new("ns", "col", "only_b")]);
        assert!(a.get(&RecordKey::new("ns", "col", "only_b")).await.unwrap().is_some());
        assert!(b.get(&RecordKey::new("ns", "col", "only_a")).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn append_only_divergence_auto_merges() {
        let a = MemStore::default();
        let b = MemStore::default();
        let _ = a.put(rec("t", RecordKind::Topic, "base\nfrom_a\n"), None).await.unwrap();
        let _ = b.put(rec("t", RecordKind::Topic, "base\nfrom_b\n"), None).await.unwrap();

        let report = sync(&a, &b).await.unwrap();
        assert_eq!(report.merged, vec![RecordKey::new("ns", "col", "t")]);
        assert!(report.conflicts.is_empty());
        let merged = a.get(&RecordKey::new("ns", "col", "t")).await.unwrap().unwrap();
        let text = String::from_utf8(merged.body.bytes().to_vec()).unwrap();
        assert!(text.contains("from_a") && text.contains("from_b") && text.contains("base"));
        // Both stores converge to the same revision.
        let mb = b.get(&RecordKey::new("ns", "col", "t")).await.unwrap().unwrap();
        assert_eq!(merged.revision, mb.revision);
    }

    #[tokio::test]
    async fn checkpoint_divergence_surfaces_conflict() {
        let a = MemStore::default();
        let b = MemStore::default();
        let _ = a.put(rec("c", RecordKind::Checkpoint, "a"), None).await.unwrap();
        let _ = b.put(rec("c", RecordKind::Checkpoint, "b"), None).await.unwrap();

        let report = sync(&a, &b).await.unwrap();
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.conflicts[0].key, RecordKey::new("ns", "col", "c"));
        assert!(report.merged.is_empty());
    }
}
```

- [ ] **Step 4: Verify**

Run: `cargo test -p gonzalo-core` — all pass (3 new sync tests + existing).
Run: `cargo clippy -p gonzalo-core --all-targets --features conformance -- -D warnings` — clean. Fix any unused-import warnings.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "$(printf 'feat(core): add Sync engine reconciling two Stores\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 3: `gonzalo-store-git` crate

**Files:**
- Create: `crates/gonzalo-store-git/Cargo.toml`
- Create: `crates/gonzalo-store-git/src/lib.rs`
- Create: `crates/gonzalo-store-git/tests/conformance.rs`
- Modify: root `Cargo.toml` (workspace members + add `git2` to `[workspace.dependencies]`)

- [ ] **Step 1: Add to workspace.** In root `Cargo.toml`, add `"crates/gonzalo-store-git"` to `members`, and under `[workspace.dependencies]` add:
```toml
git2 = { version = "0.19", default-features = false }
```

- [ ] **Step 2: `crates/gonzalo-store-git/Cargo.toml`**
```toml
[package]
name = "gonzalo-store-git"
description = "Git-backed storage substrate for gonzalo"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true
rust-version.workspace = true
repository.workspace = true

[dependencies]
gonzalo-core = { workspace = true }
async-trait  = { workspace = true }
serde_json   = { workspace = true }
tokio        = { workspace = true, features = ["fs", "io-util", "rt", "macros", "sync"] }
git2         = { workspace = true }

[dev-dependencies]
gonzalo-core = { workspace = true, features = ["conformance"] }
tokio        = { workspace = true }
tempfile     = { workspace = true }

[lints]
workspace = true
```

- [ ] **Step 3: Implement `GitStore`** (`crates/gonzalo-store-git/src/lib.rs`).

`GitStore` stores each record as JSON at `<root>/<ns>/<col>/<id>.json` inside a git worktree, exactly like `FsStore` (reuse `gonzalo_core::record_components`), and commits after each successful `put`. git2 is blocking, so wrap git calls in `tokio::task::spawn_blocking`. The repo is opened/created in `GitStore::open`.

Full implementation:
```rust
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
```

- [ ] **Step 4: Conformance test** (`crates/gonzalo-store-git/tests/conformance.rs`)
```rust
use gonzalo_core::conformance::run_store_conformance;
use gonzalo_store_git::GitStore;

#[tokio::test]
async fn git_store_passes_conformance() {
    run_store_conformance(|| async {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.keep();
        GitStore::open(path).expect("open git store")
    })
    .await;
}
```

- [ ] **Step 5: Verify**

Run: `cargo test -p gonzalo-store-git` — `git_store_passes_conformance` passes.
Run: `cargo clippy -p gonzalo-store-git --all-targets -- -D warnings` — clean.

> If `git2` fails to build (libgit2/openssl system deps), it is configured
> `default-features = false` to use the vendored libgit2 + rustls path; if the
> link still fails in this environment, report BLOCKED with the linker error
> rather than disabling the crate.

- [ ] **Step 6: Commit**
```bash
git add -A
git commit -m "$(printf 'feat(store-git): add git-backed Store substrate\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 4: `gonzalo-store-s3` crate

**Files:**
- Create: `crates/gonzalo-store-s3/Cargo.toml`
- Create: `crates/gonzalo-store-s3/src/lib.rs`
- Create: `crates/gonzalo-store-s3/tests/integration.rs` (gated)
- Modify: root `Cargo.toml` (members + aws deps)

- [ ] **Step 1: Add to workspace.** Add `"crates/gonzalo-store-s3"` to `members`; under `[workspace.dependencies]` add:
```toml
aws-config = { version = "1", features = ["behavior-version-latest"] }
aws-sdk-s3 = { version = "1", default-features = false, features = ["rt-tokio", "rustls"] }
```

- [ ] **Step 2: `crates/gonzalo-store-s3/Cargo.toml`**
```toml
[package]
name = "gonzalo-store-s3"
description = "S3-compatible object-store substrate for gonzalo"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true
rust-version.workspace = true
repository.workspace = true

[dependencies]
gonzalo-core = { workspace = true }
async-trait  = { workspace = true }
serde_json   = { workspace = true }
tokio        = { workspace = true, features = ["rt", "macros"] }
aws-config   = { workspace = true }
aws-sdk-s3   = { workspace = true }

[dev-dependencies]
gonzalo-core = { workspace = true, features = ["conformance"] }
tokio        = { workspace = true }

[lints]
workspace = true
```

- [ ] **Step 3: Implement `S3Store`** (`crates/gonzalo-store-s3/src/lib.rs`). Object key = `gonzalo_core::object_key(&key)`. OCC = read-current-revision then put (TOCTOU documented). `list` uses `list_objects_v2` with the `<namespace>/<collection>/` prefix when both are set, else `<namespace>/` or no prefix, then reconstructs keys by stripping `.json`.

```rust
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
                if let Some(k) = obj.key() {
                    if let Some(key) = parse_object_key(k) {
                        if prefix.matches(&key) {
                            out.push(key);
                        }
                    }
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
```

- [ ] **Step 4: Integration test (gated)** (`crates/gonzalo-store-s3/tests/integration.rs`). Runs the conformance suite against a real S3-compatible endpoint **only** when `GONZALO_S3_TEST_ENDPOINT` and `GONZALO_S3_TEST_BUCKET` env vars are set (e.g. a local MinIO); otherwise it is skipped so CI without Docker stays green.
```rust
use gonzalo_core::conformance::run_store_conformance;
use gonzalo_store_s3::S3Store;

#[tokio::test]
async fn s3_store_passes_conformance_when_endpoint_configured() {
    let (Ok(endpoint), Ok(bucket)) = (
        std::env::var("GONZALO_S3_TEST_ENDPOINT"),
        std::env::var("GONZALO_S3_TEST_BUCKET"),
    ) else {
        eprintln!("skipping: set GONZALO_S3_TEST_ENDPOINT and GONZALO_S3_TEST_BUCKET to run");
        return;
    };
    run_store_conformance(|| async {
        // Each factory call uses a unique key prefix is not supported by the
        // suite; instead assume the bucket is emptied between runs by the
        // operator. Connect fresh each time.
        S3Store::connect(bucket.clone(), Some(endpoint.clone())).await
    })
    .await;
}
```

- [ ] **Step 5: Verify**

Run: `cargo test -p gonzalo-store-s3` — unit tests pass; integration test prints the skip message (no endpoint configured here) and returns OK.
Run: `cargo clippy -p gonzalo-store-s3 --all-targets -- -D warnings` — clean.
Run: `cargo build -p gonzalo-store-s3` — compiles.

> The integration test against a live endpoint cannot be executed in this
> environment (no MinIO/Docker). The crate must still compile, the unit tests
> (key parsing) must pass, and clippy must be clean. Report this limitation
> explicitly.

- [ ] **Step 6: Commit**
```bash
git add -A
git commit -m "$(printf 'feat(store-s3): add S3-compatible object-store substrate\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 5: Facade wiring + workspace verification

**Files:**
- Modify: `crates/gonzalo/Cargo.toml` (optional git/s3 features)
- Modify: `crates/gonzalo/src/lib.rs` (re-export Sync types + feature-gated stores)

- [ ] **Step 1: Add features to `crates/gonzalo/Cargo.toml`**
```toml
[dependencies]
gonzalo-core   = { workspace = true }
gonzalo-domain = { workspace = true }
gonzalo-store-fs = { workspace = true, optional = true }
gonzalo-store-git = { workspace = true, optional = true }
gonzalo-store-s3 = { workspace = true, optional = true }

[features]
default = ["fs"]
fs = ["dep:gonzalo-store-fs"]
git = ["dep:gonzalo-store-git"]
s3 = ["dep:gonzalo-store-s3"]
```

- [ ] **Step 2: Re-export in `crates/gonzalo/src/lib.rs`**

Add to the core re-export list: `SyncConflict, SyncReport, sync`. After the `fs` cfg block add:
```rust
#[cfg(feature = "git")]
pub use gonzalo_store_git::GitStore;

#[cfg(feature = "s3")]
pub use gonzalo_store_s3::S3Store;
```

- [ ] **Step 3: Workspace verification**

Run: `cargo build --workspace`
Run: `cargo test --workspace`
Run: `cargo build -p gonzalo --features "git s3"` (facade with all substrates)
Run: `cargo clippy --workspace --all-targets --features gonzalo-core/conformance -- -D warnings` — clean
Run: `cargo fmt --all -- --check` — clean (run `cargo fmt --all` first if needed)

- [ ] **Step 4: Commit + push**
```bash
git add -A
git commit -m "$(printf 'feat(gonzalo): wire git/s3 substrates and Sync into facade\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
git push
```

---

## Self-Review

- §3 substrates: `gonzalo-store-git` (Task 3), `gonzalo-store-s3` (Task 4) — both implement the generic `Store` and pass (git) / are structured to pass (s3) the conformance suite. ✓
- §6 Sync: `gonzalo-core::sync` (Task 2) reconciles two stores, auto-merges append-only, surfaces conflicts. ✓
- DRY: shared `paths` module (Task 1) used by fs/git/s3. ✓
- Known limits (documented, not gaps): s3 live conformance needs an external endpoint; git pull is FF-only; sync assumes quiescent stores and uses empty-base merge (correct for append-only). All deferred refinements noted in-code.
- Placeholder scan: none. Type consistency: `Store`/`PutResult`/`Conflict(Box<_>)`/`record_components`/`object_key`/`segment` used consistently across crates.
