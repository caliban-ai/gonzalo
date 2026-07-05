//! Command implementations for the gonzalo admin CLI.

use anyhow::Context;
use anyhow::Result;
use gonzalo_core::{
    BlobStore, Body, ContentHash, Identity, KeyPrefix, Manifest, Meta, PutResult, Record,
    RecordKey, RecordKind, Revision, Store, segment,
};
use gonzalo_graph::{CodeGraph, GraphStore, Language, build};
use gonzalo_graph_sqlite::{SqliteGraphStore, view_db_path};
use gonzalo_parse::ParserPool;
use gonzalo_store_fs::FsStore;
use gonzalo_ticket::IngestSummary;
use gonzalo_ticket_config::{Config, Connection, parse_category};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ─── list ────────────────────────────────────────────────────────────────────

/// Return all record keys in the store, optionally filtered by namespace /
/// collection.
pub async fn list(
    root: &Path,
    namespace: Option<String>,
    collection: Option<String>,
) -> Result<Vec<RecordKey>> {
    let store = FsStore::new(root);
    let prefix = KeyPrefix {
        namespace,
        collection,
    };
    let keys = store.list(&prefix).await?;
    Ok(keys)
}

// ─── get ─────────────────────────────────────────────────────────────────────

/// Fetch a single record, or `None` if it does not exist.
pub async fn get(root: &Path, ns: &str, col: &str, id: &str) -> Result<Option<Record>> {
    let store = FsStore::new(root);
    let key = RecordKey::new(ns, col, id);
    Ok(store.get(&key).await?)
}

// ─── status ──────────────────────────────────────────────────────────────────

/// Count of records grouped by `"namespace/collection"`.
pub async fn status(root: &Path) -> Result<BTreeMap<String, usize>> {
    let keys = list(root, None, None).await?;
    let mut map: BTreeMap<String, usize> = BTreeMap::new();
    for k in keys {
        *map.entry(format!("{}/{}", k.namespace, k.collection))
            .or_insert(0) += 1;
    }
    Ok(map)
}

// ─── migrate ─────────────────────────────────────────────────────────────────

/// Summary returned by [`migrate`].
pub struct MigrateSummary {
    pub imported: usize,
    pub skipped: usize,
}

/// Recursively import every file under `src` as a record in the fs store at
/// `root`. Idempotent: if the key already exists, skip it.
pub async fn migrate(
    root: &Path,
    src: &Path,
    namespace: &str,
    collection: &str,
    kind: RecordKind,
) -> Result<MigrateSummary> {
    let store = FsStore::new(root);
    let mut imported = 0usize;
    let mut skipped = 0usize;

    // Collect all file paths recursively using std::fs (no walkdir dep).
    let files = collect_files(src)?;

    for abs_path in files {
        // Build relative path string with `/` as separator.
        let rel = abs_path
            .strip_prefix(src)
            .map_err(|e| anyhow::anyhow!("strip_prefix failed: {e}"))?;
        let rel_str = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");

        let id = segment(&rel_str);
        let key = RecordKey::new(namespace, collection, id);

        // Idempotency: skip if already present.
        if store.get(&key).await?.is_some() {
            skipped += 1;
            continue;
        }

        let file_bytes = std::fs::read(&abs_path)?;
        let body = Body::Inline(file_bytes);
        let record = Record {
            key,
            kind,
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            meta: Meta {
                author: Identity::new("gonzalo-cli"),
                origin_system: "migrate".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: vec![],
        };

        match store.put(record, None).await? {
            PutResult::Committed(_) => imported += 1,
            PutResult::Conflict(_) => skipped += 1,
        }
    }

    Ok(MigrateSummary { imported, skipped })
}

/// Walk `dir` recursively and return a sorted list of all file paths.
fn collect_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    collect_files_inner(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_files_inner(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect_files_inner(&entry.path(), out)?;
        } else if ft.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

// ─── index ───────────────────────────────────────────────────────────────────

/// Summary returned by [`index`].
pub struct IndexSummary {
    /// Source files parsed into slices.
    pub files: usize,
    /// Paths newly added to the view.
    pub added: usize,
    /// Paths whose slice content changed.
    pub modified: usize,
    /// Paths removed from the view since the last index.
    pub deleted: usize,
    /// Files skipped because an isolated parse worker crashed or hung on them
    /// (only possible when parsing through the pool).
    pub skipped: usize,
    /// Whether this run used the git-diff-driven incremental driver (only the
    /// changed set re-parsed) rather than the full tree walk.
    pub incremental: bool,
}

/// Locate the `gonzalo-parse-worker` binary for crash-isolated parsing:
/// `GONZALO_PARSE_WORKER` env override, else a sibling of the current
/// executable (installed/`cargo build` layout). Returns `None` when no worker is
/// available, in which case indexing parses in-process.
fn resolve_parse_worker() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GONZALO_PARSE_WORKER") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let sibling = std::env::current_exe()
        .ok()?
        .with_file_name(if cfg!(windows) {
            "gonzalo-parse-worker.exe"
        } else {
            "gonzalo-parse-worker"
        });
    sibling.exists().then_some(sibling)
}

/// Parse one file's `content` as `language`, through the crash-isolated `pool`
/// if one is available (a crash/hang yields `None` — skip the file), or
/// in-process otherwise.
async fn parse_file(
    pool: Option<&ParserPool>,
    language: Language,
    content: &str,
) -> Option<CodeGraph> {
    match pool {
        Some(pool) => match pool.parse(language, content).await {
            Ok(graph) => Some(graph),
            Err(e) => {
                eprintln!("gonzalo index: skipping a file — parse worker error: {e}");
                None
            }
        },
        None => Some(build(language, content)),
    }
}

/// Index the source files under `src` into the `(repo, view)` code-graph view:
/// parse each file into a content-addressed slice, then reconcile the view's
/// manifest to the tree (ADR 0012). Re-running updates the view.
///
/// When `src` is the root of a git repo and a base commit was recorded on a
/// prior run, the changed set is sourced directly from `git diff` (only the
/// added/modified files are re-parsed and deleted files dropped) — the
/// incremental driver of gonzalo#93. Otherwise the full tree is walked. Either
/// way the manifest is reconciled as a set, so a full walk always converges the
/// view even if an incremental run ever missed a change. Slices orphaned by
/// deletions are left for a separate GC pass (which must see all live views).
pub async fn index(root: &Path, src: &Path, repo: &str, view: &str) -> Result<IndexSummary> {
    let store = FsStore::new(root);

    // Parse through a crash-isolated worker pool when a worker binary is
    // available (so a grammar crash on one file skips that file instead of
    // aborting the index); otherwise parse in-process.
    let pool = resolve_parse_worker().map(|bin| {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 8);
        ParserPool::new(bin, workers, std::time::Duration::from_secs(30))
    });

    // The persistent per-view graph, queried by the daemon without re-assembly.
    let db_path = view_db_path(&root.join("graphs"), repo, view);
    let mut graph = SqliteGraphStore::open(&db_path)
        .with_context(|| format!("opening graph db for {repo}/{view}"))?;

    // Load the view's current manifest (empty if new).
    let key = Manifest::key(repo, view);
    let existing = store.get(&key).await?;
    let current = match &existing {
        Some(rec) => Manifest::from_body(&rec.body)?,
        None => Manifest::new(),
    };

    // Choose the driver: incremental when `src` is a git repo root, a base was
    // recorded, and the diff against it is readable; otherwise a full walk.
    let base_path = db_path.with_extension("base");
    let recorded_base = std::fs::read_to_string(&base_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let incremental_changed = recorded_base
        .as_deref()
        .and_then(|base| gonzalo_store_git::changed_paths(src, base).ok());

    let (desired, files, skipped, incremental) = match incremental_changed {
        Some(changed) => {
            build_desired_incremental(&store, &mut graph, pool.as_ref(), src, &current, &changed)
                .await?
        }
        None => build_desired_full(&store, &mut graph, pool.as_ref(), src).await?,
    };

    // Reconcile against the current manifest and drop removed paths from the
    // persistent graph.
    let recon = current.reconcile(&desired);
    for path in &recon.deleted {
        graph.remove_path(path);
    }

    // Write the manifest record (create-or-update under OCC).
    let body = recon.manifest.to_body();
    let (revision, expected, parent) = match &existing {
        Some(rec) => (
            rec.revision.next(body.bytes()),
            Some(rec.revision.clone()),
            Some(rec.revision.clone()),
        ),
        None => (Revision::initial(body.bytes()), None, None),
    };
    let record = Record {
        key,
        kind: RecordKind::GraphManifest,
        revision,
        parent,
        body,
        meta: Meta {
            author: Identity::new("gonzalo-index"),
            origin_system: "gonzalo-index".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
    };
    match store.put(record, expected).await? {
        PutResult::Committed(_) => {}
        PutResult::Conflict(_) => {
            anyhow::bail!("manifest for {repo}/{view} changed concurrently; retry the index")
        }
    }

    // Record the current HEAD as the base for the next run's incremental diff.
    // Only succeeds when `src` is a git repo root with at least one commit.
    if let Ok(sha) = gonzalo_store_git::head_commit(src) {
        if let Some(parent) = base_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&base_path, sha).ok();
    }

    Ok(IndexSummary {
        files,
        added: recon.added.len(),
        modified: recon.modified.len(),
        deleted: recon.deleted.len(),
        skipped,
        incremental,
    })
}

/// Full-walk desired set: parse every supported source file under `src`.
async fn build_desired_full(
    store: &FsStore,
    graph: &mut SqliteGraphStore,
    pool: Option<&ParserPool>,
    src: &Path,
) -> Result<(BTreeMap<String, ContentHash>, usize, usize, bool)> {
    let mut desired: BTreeMap<String, ContentHash> = BTreeMap::new();
    let mut skipped = 0usize;
    for (path, language) in source_files(src)? {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let rel = path
            .strip_prefix(src)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let Some(slice) = parse_file(pool, language, &content).await else {
            skipped += 1;
            continue;
        };
        let hash = store.put_blob(&slice.to_slice_bytes()).await?;
        graph.insert(&rel, slice); // insert replaces this path's rows
        desired.insert(rel, hash);
    }
    let files = desired.len();
    Ok((desired, files, skipped, false))
}

/// Incremental desired set: start from the current manifest and apply only the
/// git-reported changes — re-parse added/modified source files, drop deleted
/// ones, and carry every unchanged path forward untouched. `files` counts the
/// files re-parsed this run.
async fn build_desired_incremental(
    store: &FsStore,
    graph: &mut SqliteGraphStore,
    pool: Option<&ParserPool>,
    src: &Path,
    current: &Manifest,
    changed: &gonzalo_store_git::ChangedPaths,
) -> Result<(BTreeMap<String, ContentHash>, usize, usize, bool)> {
    let mut desired = current.entries.clone();
    let mut files = 0usize;
    let mut skipped = 0usize;

    for rel in changed.added.iter().chain(changed.modified.iter()) {
        if !is_indexable(rel) {
            continue;
        }
        let Some(language) = Path::new(rel)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Language::from_extension)
        else {
            continue; // not a source file
        };
        // A file git reports as changed but that we can no longer read (e.g.
        // it vanished between diff and read) is treated as a removal.
        let content = match std::fs::read_to_string(src.join(rel)) {
            Ok(c) => c,
            Err(_) => {
                if desired.remove(rel).is_some() {
                    graph.remove_path(rel);
                }
                continue;
            }
        };
        let Some(slice) = parse_file(pool, language, &content).await else {
            skipped += 1;
            continue;
        };
        let hash = store.put_blob(&slice.to_slice_bytes()).await?;
        graph.insert(rel, slice);
        desired.insert(rel.clone(), hash);
        files += 1;
    }

    for rel in &changed.deleted {
        if desired.remove(rel).is_some() {
            graph.remove_path(rel);
        }
    }

    Ok((desired, files, skipped, true))
}

/// Whether a repo-relative path is eligible for indexing — mirrors the
/// full-walk skip of `target`, `.git`, and hidden directories so the git driver
/// and the full walk agree on which paths belong to a view.
fn is_indexable(rel: &str) -> bool {
    !rel.split('/')
        .any(|part| part == "target" || part == ".git" || part.starts_with('.'))
}

/// Supported source files under `dir` with their [`Language`], sorted by path,
/// skipping `target`, `.git`, and hidden directories (build artifacts and VCS
/// internals are not source). Files whose extension maps to no language are
/// skipped.
fn source_files(dir: &Path) -> Result<Vec<(PathBuf, Language)>> {
    let mut out = Vec::new();
    source_files_inner(dir, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn source_files_inner(dir: &Path, out: &mut Vec<(PathBuf, Language)>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().to_string();
        if ft.is_dir() {
            if name == "target" || name == ".git" || name.starts_with('.') {
                continue;
            }
            source_files_inner(&entry.path(), out)?;
        } else if ft.is_file() {
            let path = entry.path();
            if let Some(language) = path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(Language::from_extension)
            {
                out.push((path, language));
            }
        }
    }
    Ok(())
}

// ─── gc ────────────────────────────────────────────────────────────────────

/// Summary returned by [`gc`].
pub struct GcSummary {
    /// Live manifests scanned to build the mark set.
    pub manifests: usize,
    /// Orphaned slice blobs deleted.
    pub freed: usize,
    /// Slice blobs kept because some live view still references them.
    pub retained: usize,
}

/// Sweep orphaned code-graph slices from the store at `root`.
///
/// Slices are content-addressed and **shared across views** (identical content
/// dedups), so GC must mark against *every* live view's manifest — deleting a
/// slice still referenced by another view would corrupt it. This enumerates all
/// `graph-manifest` records across every repo/view, unions their referenced
/// hashes, and mark-sweeps the blob store via [`gonzalo_core::gc_blobs`] (A6).
pub async fn gc(root: &Path) -> Result<GcSummary> {
    let store = FsStore::new(root);

    // Every view's manifest, across all repos (namespace unset = all repos).
    let prefix = KeyPrefix {
        namespace: None,
        collection: Some(Manifest::collection().to_string()),
    };
    let keys = store.list(&prefix).await?;
    let mut manifests = Vec::with_capacity(keys.len());
    for key in &keys {
        if let Some(rec) = store.get(key).await? {
            manifests.push(Manifest::from_body(&rec.body)?);
        }
    }

    let report = gonzalo_core::gc_blobs(&store, &manifests).await?;
    Ok(GcSummary {
        manifests: manifests.len(),
        freed: report.freed.len(),
        retained: report.retained,
    })
}

/// [`index`] the `(repo, view)` view, then — when `gc_after` — sweep orphaned
/// slices. The opt-in post-index trigger of gonzalo#104: the sweep runs only
/// after a successful index and always goes through [`gc`], which marks against
/// *every* live view's manifest (never a per-view subset), so a slice the just-
/// indexed view dropped but another view still references is preserved.
pub async fn index_with_gc(
    root: &Path,
    src: &Path,
    repo: &str,
    view: &str,
    gc_after: bool,
) -> Result<(IndexSummary, Option<GcSummary>)> {
    let summary = index(root, src, repo, view).await?;
    let swept = if gc_after {
        Some(gc(root).await?)
    } else {
        None
    };
    Ok((summary, swept))
}

// ─── sync_stores ─────────────────────────────────────────────────────────────

/// Summary returned by [`sync_stores`].
pub struct SyncSummary {
    pub copied_to_a: usize,
    pub copied_to_b: usize,
    pub merged: usize,
    pub conflicts: usize,
}

/// Sync two filesystem stores via [`gonzalo_core::sync`].
pub async fn sync_stores(a: &Path, b: &Path) -> Result<SyncSummary> {
    let store_a = FsStore::new(a);
    let store_b = FsStore::new(b);
    let report = gonzalo_core::sync(&store_a, &store_b).await?;
    Ok(SyncSummary {
        copied_to_a: report.copied_to_a.len(),
        copied_to_b: report.copied_to_b.len(),
        merged: report.merged.len(),
        conflicts: report.conflicts.len(),
    })
}

// ─── ticket sync ───────────────────────────────────────────────────────────

/// Per-connection ingest result.
pub struct TicketSyncReport {
    pub connection: String,
    pub summary: IngestSummary,
}

/// Load the ticket config, build each connection's source, and ingest its
/// tickets into the fs store at `root`.
pub async fn ticket_sync(
    config_path: &Path,
    root: &Path,
    author: &str,
) -> Result<Vec<TicketSyncReport>> {
    let config = Config::load(config_path).context("loading ticket config")?;
    let store = FsStore::new(root);
    let mut reports = Vec::new();
    for (name, source) in config.sources().context("building ticket sources")? {
        let summary = gonzalo_ticket::ingest(source.as_ref(), &store, author)
            .await
            .with_context(|| format!("syncing connection {name}"))?;
        reports.push(TicketSyncReport {
            connection: name,
            summary,
        });
    }
    Ok(reports)
}

// ─── ticket move ─────────────────────────────────────────────────────────────

/// Move a board card to the column for `category`. Selects the connection named
/// `connection`, or the sole connection if there is exactly one.
pub async fn ticket_move(
    config_path: &Path,
    connection: Option<&str>,
    uid: &str,
    category: &str,
) -> Result<()> {
    let cat = parse_category(category)
        .ok_or_else(|| anyhow::anyhow!("unknown state category {category:?}"))?;
    let config = Config::load(config_path).context("loading ticket config")?;
    let conn = select_connection(&config.connections, connection)?;
    let source = gonzalo_ticket_config::build_source(conn).context("building ticket source")?;
    source
        .set_state(uid, cat)
        .await
        .with_context(|| format!("moving {uid} to {category}"))?;
    Ok(())
}

/// Pick the requested connection by name, or the only one if unambiguous.
fn select_connection<'a>(
    connections: &'a [Connection],
    name: Option<&str>,
) -> Result<&'a Connection> {
    match name {
        Some(n) => connections
            .iter()
            .find(|c| c.name == n)
            .ok_or_else(|| anyhow::anyhow!("no connection named {n:?}")),
        None => match connections {
            [one] => Ok(one),
            [] => Err(anyhow::anyhow!("no connections configured")),
            _ => Err(anyhow::anyhow!(
                "multiple connections configured; pass --connection <name>"
            )),
        },
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_graph::GraphStore;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    // ── migrate: basic import ────────────────────────────────────────────────

    #[tokio::test]
    async fn migrate_imports_two_files() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();

        write_file(src.path(), "alpha.md", "hello alpha");
        write_file(src.path(), "beta.md", "hello beta");

        let summary = migrate(
            root.path(),
            src.path(),
            "testns",
            "testcol",
            RecordKind::Topic,
        )
        .await
        .unwrap();

        assert_eq!(summary.imported, 2, "should have imported 2 files");
        assert_eq!(summary.skipped, 0, "nothing should be skipped yet");
    }

    // ── list: shows the right keys after migrate ─────────────────────────────

    #[tokio::test]
    async fn list_returns_migrated_keys() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();

        write_file(src.path(), "alpha.md", "hello alpha");
        write_file(src.path(), "beta.md", "hello beta");

        migrate(
            root.path(),
            src.path(),
            "testns",
            "testcol",
            RecordKind::Topic,
        )
        .await
        .unwrap();

        let keys = list(root.path(), None, None).await.unwrap();
        assert_eq!(keys.len(), 2);
    }

    // ── migrate: idempotent on second run ────────────────────────────────────

    #[tokio::test]
    async fn migrate_is_idempotent() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();

        write_file(src.path(), "alpha.md", "hello alpha");
        write_file(src.path(), "beta.md", "hello beta");

        migrate(
            root.path(),
            src.path(),
            "testns",
            "testcol",
            RecordKind::Topic,
        )
        .await
        .unwrap();

        let second = migrate(
            root.path(),
            src.path(),
            "testns",
            "testcol",
            RecordKind::Topic,
        )
        .await
        .unwrap();

        assert_eq!(
            second.skipped, 2,
            "second run should skip both already-imported files"
        );
        assert_eq!(second.imported, 0, "second run should import nothing new");
    }

    // ── get: round-trips body ────────────────────────────────────────────────

    #[tokio::test]
    async fn get_returns_migrated_record_body() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();

        write_file(src.path(), "alpha.md", "hello alpha");

        migrate(
            root.path(),
            src.path(),
            "testns",
            "testcol",
            RecordKind::Topic,
        )
        .await
        .unwrap();

        // The id is segment("alpha.md") = "alpha_md"
        let record = get(root.path(), "testns", "testcol", "alpha_md")
            .await
            .unwrap();

        assert!(record.is_some(), "record should be present");
        let body = record.unwrap().body;
        assert_eq!(body.bytes(), b"hello alpha");
    }

    // ── status: correct namespace/collection count ───────────────────────────

    #[tokio::test]
    async fn status_groups_by_ns_col() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();

        write_file(src.path(), "alpha.md", "hello alpha");
        write_file(src.path(), "beta.md", "hello beta");

        migrate(
            root.path(),
            src.path(),
            "testns",
            "testcol",
            RecordKind::Topic,
        )
        .await
        .unwrap();

        let map = status(root.path()).await.unwrap();
        assert_eq!(map.get("testns/testcol").copied(), Some(2));
    }

    // ── sync_stores: propagates records ─────────────────────────────────────

    #[tokio::test]
    async fn sync_stores_copies_to_b() {
        let store_a = TempDir::new().unwrap();
        let store_b = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();

        write_file(src.path(), "note.md", "synced content");

        // Import only into store A.
        migrate(
            store_a.path(),
            src.path(),
            "testns",
            "testcol",
            RecordKind::Topic,
        )
        .await
        .unwrap();

        let summary = sync_stores(store_a.path(), store_b.path()).await.unwrap();
        assert_eq!(summary.copied_to_b, 1);

        // Store B should now have the key.
        let keys = list(store_b.path(), None, None).await.unwrap();
        assert_eq!(keys.len(), 1);
    }

    // ── ticket_sync: empty config → no reports ───────────────────────────────

    #[tokio::test]
    async fn ticket_sync_with_no_connections_returns_no_reports() {
        let root = TempDir::new().unwrap();
        let cfg = TempDir::new().unwrap();
        let cfg_path = cfg.path().join("tickets.toml");
        std::fs::write(&cfg_path, "").unwrap(); // empty config = zero connections

        let reports = ticket_sync(&cfg_path, root.path(), "tester").await.unwrap();
        assert!(reports.is_empty());
    }

    // ── ticket_move: unknown category errors before any network call ─────────

    #[tokio::test]
    async fn ticket_move_unknown_category_errors() {
        let cfg = TempDir::new().unwrap();
        let cfg_path = cfg.path().join("tickets.toml");
        std::fs::write(
            &cfg_path,
            "[[connection]]\nname=\"b\"\nprovider=\"github-projects\"\norg=\"o\"\nproject=1\ntoken_env=\"X\"\n",
        )
        .unwrap();
        // "frozen" is not a valid category → error before any network call.
        let err = ticket_move(&cfg_path, None, "o/r#1", "frozen")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("category"), "got {err}");
    }

    // ── ticket_move: ambiguous connection requires --connection ─────────────

    #[tokio::test]
    async fn ticket_move_requires_connection_when_many() {
        let cfg = TempDir::new().unwrap();
        let cfg_path = cfg.path().join("tickets.toml");
        std::fs::write(
            &cfg_path,
            "[[connection]]\nname=\"a\"\nprovider=\"github-projects\"\norg=\"o\"\nproject=1\ntoken_env=\"X\"\n\
             [[connection]]\nname=\"b\"\nprovider=\"github-projects\"\norg=\"o\"\nproject=2\ntoken_env=\"Y\"\n",
        )
        .unwrap();
        let err = ticket_move(&cfg_path, None, "o/r#1", "done")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("connection"), "got {err}");
    }

    // ── index: build a queryable view from a source tree ─────────────────────

    #[tokio::test]
    async fn index_builds_a_queryable_view() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "a.rs", "fn helper() {}");
        std::fs::create_dir(src.path().join("sub")).unwrap();
        write_file(src.path(), "sub/b.rs", "fn caller() { helper(); }");

        let summary = index(root.path(), src.path(), "r", "main").await.unwrap();
        assert_eq!(summary.files, 2);
        assert_eq!(summary.added, 2);
        assert_eq!(summary.modified, 0);
        assert_eq!(summary.deleted, 0);

        // The indexed view assembles and answers real queries.
        let store = FsStore::new(root.path());
        let manifest = Manifest::from_body(
            &store
                .get(&Manifest::key("r", "main"))
                .await
                .unwrap()
                .unwrap()
                .body,
        )
        .unwrap();
        let graph = gonzalo_graph::assemble(&manifest, &store).await.unwrap();
        let defs = graph.definitions("helper");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].path, "a.rs");
        assert_eq!(graph.callers_of("helper"), vec!["caller".to_string()]);
        assert!(
            graph
                .symbols_in_file("sub/b.rs")
                .iter()
                .any(|s| s.name == "caller")
        );
    }

    #[tokio::test]
    async fn reindex_reports_modifications_and_deletions() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "keep.rs", "fn keep() {}");
        write_file(src.path(), "gone.rs", "fn gone() {}");
        index(root.path(), src.path(), "r", "v").await.unwrap();

        // Change one file, remove another.
        write_file(src.path(), "keep.rs", "fn keep() { extra(); }");
        std::fs::remove_file(src.path().join("gone.rs")).unwrap();

        let summary = index(root.path(), src.path(), "r", "v").await.unwrap();
        assert_eq!(summary.files, 1);
        assert_eq!(summary.added, 0);
        assert_eq!(summary.modified, 1);
        assert_eq!(summary.deleted, 1);
    }

    #[tokio::test]
    async fn index_skips_non_rust_and_build_dirs() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "real.rs", "fn real() {}");
        write_file(src.path(), "notes.txt", "not source");
        std::fs::create_dir(src.path().join("target")).unwrap();
        write_file(src.path(), "target/gen.rs", "fn generated() {}");

        let summary = index(root.path(), src.path(), "r", "v").await.unwrap();
        assert_eq!(summary.files, 1, "only real.rs is indexed");
        assert_eq!(summary.added, 1);
    }

    #[tokio::test]
    async fn index_handles_multiple_languages() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "lib.rs", "fn rust_fn() {}");
        write_file(src.path(), "app.py", "def py_fn():\n    pass\n");

        let summary = index(root.path(), src.path(), "r", "main").await.unwrap();
        assert_eq!(summary.files, 2, "both the .rs and .py file are indexed");

        // Both languages' symbols are queryable in the assembled view.
        let store = FsStore::new(root.path());
        let manifest = Manifest::from_body(
            &store
                .get(&Manifest::key("r", "main"))
                .await
                .unwrap()
                .unwrap()
                .body,
        )
        .unwrap();
        let graph = gonzalo_graph::assemble(&manifest, &store).await.unwrap();
        assert_eq!(graph.definitions("rust_fn")[0].path, "lib.rs");
        assert_eq!(graph.definitions("py_fn")[0].path, "app.py");
    }

    #[tokio::test]
    async fn index_writes_a_queryable_sqlite_graph() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(
            src.path(),
            "lib.rs",
            "fn helper() {}\nfn main() { helper(); }",
        );
        index(root.path(), src.path(), "r", "main").await.unwrap();

        // The persistent per-view graph exists under <root>/graphs and answers
        // queries without re-assembly.
        let db = view_db_path(&root.path().join("graphs"), "r", "main");
        let g = SqliteGraphStore::open(&db).unwrap();
        assert_eq!(g.definitions("helper")[0].path, "lib.rs");
        assert_eq!(g.callers_of("helper"), vec!["main".to_string()]);
    }

    #[tokio::test]
    async fn reindex_removes_deleted_paths_from_the_sqlite_graph() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "keep.rs", "fn keep() {}");
        write_file(src.path(), "gone.rs", "fn gone() {}");
        index(root.path(), src.path(), "r", "v").await.unwrap();

        std::fs::remove_file(src.path().join("gone.rs")).unwrap();
        index(root.path(), src.path(), "r", "v").await.unwrap();

        let g =
            SqliteGraphStore::open(view_db_path(&root.path().join("graphs"), "r", "v")).unwrap();
        assert_eq!(g.definitions("keep").len(), 1);
        assert!(
            g.definitions("gone").is_empty(),
            "deleted file's symbols must be gone from the graph"
        );
    }

    // ── index: git-driven incremental sync (gonzalo#93) ──────────────────────

    /// Init a git repo at `dir` and commit every current file.
    fn git_init_commit(dir: &Path) {
        let repo = git2::Repository::init(dir).unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@localhost").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "c", &tree, &[])
            .unwrap();
    }

    #[tokio::test]
    async fn first_index_of_git_repo_is_full_then_reindex_is_incremental() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "a.rs", "fn a() {}");
        write_file(src.path(), "b.rs", "fn b() {}");
        git_init_commit(src.path());

        // First run: no recorded base yet → full walk, records the base.
        let first = index(root.path(), src.path(), "r", "main").await.unwrap();
        assert!(!first.incremental, "first index is a full walk");
        assert_eq!(first.files, 2);
        assert_eq!(first.added, 2);

        // Change the working tree: modify a.rs, add untracked c.rs, leave b.rs.
        write_file(src.path(), "a.rs", "fn a() { helper(); }");
        write_file(src.path(), "c.rs", "fn c() {}");

        let second = index(root.path(), src.path(), "r", "main").await.unwrap();
        assert!(second.incremental, "second index uses the git diff driver");
        assert_eq!(second.files, 2, "only a.rs and c.rs are re-parsed");
        assert_eq!(second.added, 1, "c.rs added");
        assert_eq!(second.modified, 1, "a.rs modified");
        assert_eq!(second.deleted, 0);

        // b.rs was carried forward unchanged and is still queryable.
        let g =
            SqliteGraphStore::open(view_db_path(&root.path().join("graphs"), "r", "main")).unwrap();
        assert_eq!(g.definitions("b")[0].path, "b.rs");
        assert_eq!(g.definitions("c")[0].path, "c.rs");
    }

    #[tokio::test]
    async fn incremental_index_drops_deleted_files() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "keep.rs", "fn keep() {}");
        write_file(src.path(), "gone.rs", "fn gone() {}");
        git_init_commit(src.path());
        index(root.path(), src.path(), "r", "v").await.unwrap();

        std::fs::remove_file(src.path().join("gone.rs")).unwrap();
        let summary = index(root.path(), src.path(), "r", "v").await.unwrap();
        assert!(summary.incremental);
        assert_eq!(summary.deleted, 1);

        let g =
            SqliteGraphStore::open(view_db_path(&root.path().join("graphs"), "r", "v")).unwrap();
        assert_eq!(g.definitions("keep").len(), 1);
        assert!(g.definitions("gone").is_empty(), "deleted file is gone");
    }

    #[tokio::test]
    async fn non_git_src_stays_full_walk() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "a.rs", "fn a() {}");
        let summary = index(root.path(), src.path(), "r", "main").await.unwrap();
        assert!(!summary.incremental, "a non-git tree cannot go incremental");
    }

    // ── gc: sweep orphaned slices across all live views (gonzalo#94) ─────────

    #[tokio::test]
    async fn gc_on_empty_store_frees_nothing() {
        let root = TempDir::new().unwrap();
        let summary = gc(root.path()).await.unwrap();
        assert_eq!(summary.manifests, 0);
        assert_eq!(summary.freed, 0);
        assert_eq!(summary.retained, 0);
    }

    #[tokio::test]
    async fn gc_frees_slices_orphaned_by_a_reindex() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "a.rs", "fn a() {}");
        index(root.path(), src.path(), "r", "main").await.unwrap();

        // Reindex with different content: the original slice is now orphaned.
        write_file(src.path(), "a.rs", "fn a() { b(); }");
        index(root.path(), src.path(), "r", "main").await.unwrap();

        let summary = gc(root.path()).await.unwrap();
        assert_eq!(summary.manifests, 1);
        assert_eq!(summary.freed, 1, "the pre-edit slice is unreferenced");
        assert_eq!(summary.retained, 1, "the current slice stays");

        // GC did not corrupt the live view.
        let g =
            SqliteGraphStore::open(view_db_path(&root.path().join("graphs"), "r", "main")).unwrap();
        assert_eq!(g.definitions("a")[0].path, "a.rs");
    }

    #[tokio::test]
    async fn index_with_gc_sweeps_orphans_when_enabled() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "a.rs", "fn a() {}");
        index(root.path(), src.path(), "r", "main").await.unwrap();

        // Reindex changed content with the post-index sweep on: the pre-edit
        // slice is orphaned and should be freed in the same call.
        write_file(src.path(), "a.rs", "fn a() { b(); }");
        let (_summary, swept) = index_with_gc(root.path(), src.path(), "r", "main", true)
            .await
            .unwrap();
        let swept = swept.expect("gc runs when enabled");
        assert_eq!(swept.freed, 1, "orphaned slice swept during the index");

        // A follow-up gc finds nothing left to free.
        assert_eq!(gc(root.path()).await.unwrap().freed, 0);
    }

    #[tokio::test]
    async fn index_with_gc_skips_sweep_when_disabled() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "a.rs", "fn a() {}");
        index(root.path(), src.path(), "r", "main").await.unwrap();

        write_file(src.path(), "a.rs", "fn a() { b(); }");
        let (_summary, swept) = index_with_gc(root.path(), src.path(), "r", "main", false)
            .await
            .unwrap();
        assert!(swept.is_none(), "no gc when disabled");

        // The orphan survived: an explicit gc still has one to free.
        assert_eq!(gc(root.path()).await.unwrap().freed, 1);
    }

    #[tokio::test]
    async fn gc_retains_slice_still_referenced_by_another_view() {
        let root = TempDir::new().unwrap();

        // Two views index the *same* file content → one shared, deduped slice.
        let src_a = TempDir::new().unwrap();
        write_file(src_a.path(), "shared.rs", "fn shared() {}");
        index(root.path(), src_a.path(), "r", "a").await.unwrap();

        let src_b = TempDir::new().unwrap();
        write_file(src_b.path(), "shared.rs", "fn shared() {}");
        index(root.path(), src_b.path(), "r", "b").await.unwrap();

        // Remove the file from view A only; view B still references the slice.
        std::fs::remove_file(src_a.path().join("shared.rs")).unwrap();
        index(root.path(), src_a.path(), "r", "a").await.unwrap();

        let summary = gc(root.path()).await.unwrap();
        assert_eq!(summary.manifests, 2);
        assert_eq!(
            summary.freed, 0,
            "the shared slice is live via view B and must not be swept"
        );

        // View B still assembles and answers with the shared symbol.
        let store = FsStore::new(root.path());
        let manifest = Manifest::from_body(
            &store
                .get(&Manifest::key("r", "b"))
                .await
                .unwrap()
                .unwrap()
                .body,
        )
        .unwrap();
        let graph = gonzalo_graph::assemble(&manifest, &store).await.unwrap();
        assert_eq!(graph.definitions("shared")[0].path, "shared.rs");
    }
}
