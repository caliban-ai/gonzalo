//! Command implementations for the gonzalo admin CLI.

use anyhow::Context;
use anyhow::Result;
use gonzalo_core::{
    BlobStore, Body, ContentHash, Identity, KeyPrefix, Manifest, Meta, PutResult, Record,
    RecordKey, RecordKind, Revision, Store,
};
use gonzalo_graph::{CodeGraph, EXTRACTION_VERSION, GraphStore, Language, build};
use gonzalo_graph_sqlite::{SqliteGraphStore, view_db_path};
use gonzalo_parse::ParserPool;
use gonzalo_store_fs::FsStore;
use gonzalo_ticket::IngestSummary;
use gonzalo_ticket_config::{Config, Connection, parse_category};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod walk;
pub use walk::{IgnoredCounts, IndexFilter};

mod watch;
pub use watch::{WatchConfig, watch};

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

        // Use the relative path verbatim as the record id. The store now
        // encodes arbitrary key characters reversibly and injectively, so two
        // distinct source files can never collide onto one record (the old
        // `segment()` collapse silently dropped one of `docs/api.md` and
        // `docs_api.md`). Ids stay human-readable (`docs/api.md`).
        let key = RecordKey::new(namespace, collection, rel_str);

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
    /// Paths excluded from the view by an [`IndexFilter`] rule — vendored or
    /// generated files, dependency/output directories, and gitignored trees
    /// (#209). Distinct from `skipped`, which is a parse failure.
    pub ignored: IgnoredCounts,
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
    index_with(root, src, repo, view, &IndexFilter::default()).await
}

/// [`index`], with control over which paths enter the view.
///
/// `filter` carries any `--include` overrides; the built-in vendored/generated
/// rules and `.gitignore` apply either way (#209).
pub async fn index_with(
    root: &Path,
    src: &Path,
    repo: &str,
    view: &str,
    filter: &IndexFilter,
) -> Result<IndexSummary> {
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
    // A view built by a parser that recorded different things must be rebuilt
    // in full: the incremental driver carries unchanged slices forward, so
    // without this an existing view keeps pre-upgrade extraction forever.
    let version_path = db_path.with_extension("fmt");
    let recorded_version: Option<u32> = std::fs::read_to_string(&version_path)
        .ok()
        .and_then(|s| s.trim().parse().ok());
    let format_changed = recorded_version != Some(EXTRACTION_VERSION);

    let recorded_base = std::fs::read_to_string(&base_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|_| !format_changed);
    let incremental_changed = recorded_base
        .as_deref()
        .and_then(|base| gonzalo_store_git::changed_paths(src, base).ok());

    // Stage every persistent-graph mutation instead of applying it inline. The
    // SqliteGraphStore is advanced only after the manifest that describes it
    // commits, so a concurrent-writer Conflict (below) leaves the graph
    // untouched rather than advanced ahead of a manifest that never landed (#153).
    let mut staging = GraphStaging::default();
    let (desired, files, skipped, ignored, incremental) = match incremental_changed {
        Some(changed) => {
            build_desired_incremental(
                &store,
                &mut staging,
                pool.as_ref(),
                src,
                &current,
                &changed,
                filter,
            )
            .await?
        }
        None => build_desired_full(&store, &mut staging, pool.as_ref(), src, filter).await?,
    };

    // Reconcile against the current manifest and stage removed paths for the
    // persistent graph (applied only after the manifest commit).
    let recon = current.reconcile(&desired);
    for path in &recon.deleted {
        staging.removes.push(path.clone());
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
            // The manifest moved under us: abandon the run WITHOUT touching the
            // persistent graph, so the SqliteGraphStore never advances ahead of a
            // committed manifest (#153). Orphaned slice blobs written above are
            // reclaimed by a later gc pass.
            anyhow::bail!("manifest for {repo}/{view} changed concurrently; retry the index")
        }
    }

    // Manifest committed — now advance the persistent graph to match it.
    staging.apply(&mut graph);

    // Record the current HEAD as the base for the next run's incremental diff.
    // Only succeeds when `src` is a git repo root with at least one commit.
    if let Ok(sha) = gonzalo_store_git::head_commit(src) {
        if let Some(parent) = base_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&base_path, sha).ok();
    }
    // Record the extraction format this view was built with, so the next run can
    // tell whether an incremental pass is still valid.
    if let Some(parent) = version_path.parent() {
        std::fs::create_dir_all(parent).ok();
        std::fs::write(&version_path, EXTRACTION_VERSION.to_string()).ok();
    }

    Ok(IndexSummary {
        files,
        added: recon.added.len(),
        modified: recon.modified.len(),
        deleted: recon.deleted.len(),
        skipped,
        ignored,
        incremental,
    })
}

/// A set of persistent-graph mutations collected during a [`index`] run but not
/// yet applied. Staging the writes lets [`index`] commit the view's manifest
/// first and only then advance the [`SqliteGraphStore`] — so a manifest Conflict
/// leaves the persistent graph untouched (#153).
#[derive(Default)]
struct GraphStaging {
    /// `(relative path, parsed slice)` to (re)insert; insert replaces the path's
    /// existing rows.
    inserts: Vec<(String, CodeGraph)>,
    /// Relative paths whose rows should be removed.
    removes: Vec<String>,
}

impl GraphStaging {
    /// Apply every staged mutation to the persistent graph. Called only after
    /// the manifest commit succeeds.
    fn apply(self, graph: &mut SqliteGraphStore) {
        for (rel, slice) in self.inserts {
            graph.insert(&rel, slice);
        }
        for rel in self.removes {
            graph.remove_path(&rel);
        }
    }
}

/// Full-walk desired set: parse every supported source file under `src` that
/// `filter` admits.
async fn build_desired_full(
    store: &FsStore,
    staging: &mut GraphStaging,
    pool: Option<&ParserPool>,
    src: &Path,
    filter: &IndexFilter,
) -> Result<(
    BTreeMap<String, ContentHash>,
    usize,
    usize,
    IgnoredCounts,
    bool,
)> {
    let mut desired: BTreeMap<String, ContentHash> = BTreeMap::new();
    let mut skipped = 0usize;
    let (sources, ignored) = walk::source_files(src, filter)?;
    for (path, language) in sources {
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
        staging.inserts.push((rel.clone(), slice)); // insert replaces this path's rows
        desired.insert(rel, hash);
    }
    let files = desired.len();
    Ok((desired, files, skipped, ignored, false))
}

/// Incremental desired set: start from the current manifest and apply only the
/// git-reported changes — re-parse added/modified source files, drop deleted
/// ones, and carry every unchanged path forward untouched. `files` counts the
/// files re-parsed this run.
async fn build_desired_incremental(
    store: &FsStore,
    staging: &mut GraphStaging,
    pool: Option<&ParserPool>,
    src: &Path,
    current: &Manifest,
    changed: &gonzalo_store_git::ChangedPaths,
    filter: &IndexFilter,
) -> Result<(
    BTreeMap<String, ContentHash>,
    usize,
    usize,
    IgnoredCounts,
    bool,
)> {
    let mut desired = current.entries.clone();
    let mut files = 0usize;
    let mut skipped = 0usize;
    // Gitignored paths never reach here — `git2`'s diff omits them — so only the
    // path-only rules apply, and only files are ever counted.
    let mut ignored = IgnoredCounts::default();

    // Re-apply the filter to paths carried forward from the previous run, not
    // just to changed ones. A view indexed under laxer rules keeps its vendored
    // bundles forever otherwise: `mermaid.min.js` never changes, so it never
    // appears in the diff, so an incremental run never reconsiders it — and once
    // a base commit is recorded there is no full walk to clean it up. Making the
    // carried-forward set self-healing is what lets an existing view benefit
    // from #209 rather than only newly created ones.
    let stale = walk::stale_entries(src, filter, desired.keys().map(String::as_str));
    for rel in stale {
        desired.remove(&rel);
        staging.removes.push(rel);
        ignored.files += 1;
    }

    for rel in changed.added.iter().chain(changed.modified.iter()) {
        if !filter.is_indexable(rel) {
            // A path that a previous, laxer walk admitted must also be dropped
            // from the view, not merely skipped, or the two drivers disagree.
            if desired.remove(rel).is_some() {
                staging.removes.push(rel.clone());
            }
            ignored.files += 1;
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
                    staging.removes.push(rel.clone());
                }
                continue;
            }
        };
        let Some(slice) = parse_file(pool, language, &content).await else {
            skipped += 1;
            continue;
        };
        let hash = store.put_blob(&slice.to_slice_bytes()).await?;
        staging.inserts.push((rel.clone(), slice));
        desired.insert(rel.clone(), hash);
        files += 1;
    }

    for rel in &changed.deleted {
        if desired.remove(rel).is_some() {
            staging.removes.push(rel.clone());
        }
    }

    Ok((desired, files, skipped, ignored, true))
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
    index_with_gc_filtered(root, src, repo, view, gc_after, &IndexFilter::default()).await
}

/// [`index_with_gc`], with control over which paths enter the view (#209).
pub async fn index_with_gc_filtered(
    root: &Path,
    src: &Path,
    repo: &str,
    view: &str,
    gc_after: bool,
    filter: &IndexFilter,
) -> Result<(IndexSummary, Option<GcSummary>)> {
    let summary = index_with(root, src, repo, view, filter).await?;
    let swept = if gc_after {
        Some(gc(root).await?)
    } else {
        None
    };
    Ok((summary, swept))
}

// ─── watch (debounce core) ──────────────────────────────────────────────────

/// Coalesces a burst of filesystem change notifications so a rapid series of
/// edits triggers a single re-index rather than one per event. The clock is
/// injected (`now`), so the logic is deterministic and unit-testable without
/// real sleeps — the seam the watcher loop drives with `Instant::now()`.
#[derive(Debug)]
pub struct Debouncer {
    window: Duration,
    /// When the most recent unhandled change was observed.
    last_event: Option<Instant>,
}

impl Debouncer {
    /// A debouncer that fires once the tree has been quiet for `window`.
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            last_event: None,
        }
    }

    /// Record that a change was observed at `now`.
    pub fn on_event(&mut self, now: Instant) {
        self.last_event = Some(now);
    }

    /// Whether a change is waiting to be handled.
    pub fn is_pending(&self) -> bool {
        self.last_event.is_some()
    }

    /// Whether a re-index is due at `now`: a change is pending and the tree has
    /// been quiet for at least `window` since the last event. False when nothing
    /// is pending.
    pub fn is_due(&self, now: Instant) -> bool {
        matches!(self.last_event, Some(t) if now.duration_since(t) >= self.window)
    }

    /// Clear the pending change after a re-index has run.
    pub fn clear(&mut self) {
        self.last_event = None;
    }
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
        // Scope each record key by connection name so the same issue on two
        // boards yields two distinct records rather than colliding (#159).
        let summary = gonzalo_ticket::ingest(source.as_ref(), &store, author, Some(&name))
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

        // The id is the source-relative path verbatim.
        let record = get(root.path(), "testns", "testcol", "alpha.md")
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

    // ── index: manifest commit precedes the SQLite graph write (gonzalo#153) ──

    #[tokio::test]
    async fn build_desired_stages_graph_writes_without_touching_the_store() {
        // The reordered control flow (#153): parsing/desired-set construction
        // must only *stage* persistent-graph mutations. The SqliteGraphStore is
        // advanced solely by GraphStaging::apply — which index() calls strictly
        // after the manifest commit — so a manifest Conflict leaves the graph
        // untouched instead of advanced ahead of an uncommitted manifest.
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "a.rs", "fn a() {}");

        let store = FsStore::new(root.path());
        let db_path = view_db_path(&root.path().join("graphs"), "r", "main");
        let mut graph = SqliteGraphStore::open(&db_path).unwrap();

        let mut staging = GraphStaging::default();
        let (desired, files, _skipped, _ignored, incremental) = build_desired_full(
            &store,
            &mut staging,
            None,
            src.path(),
            &IndexFilter::default(),
        )
        .await
        .unwrap();
        assert!(!incremental);
        assert_eq!(files, 1);
        assert_eq!(desired.len(), 1);
        assert_eq!(staging.inserts.len(), 1, "the write is staged, not applied");

        // Nothing has reached the persistent graph yet — this is the state after
        // a manifest Conflict would `bail!`.
        assert!(
            graph.definitions("a").is_empty(),
            "SqliteGraphStore must be untouched until the manifest commits"
        );

        // Applying the staged writes (index()'s post-commit step) advances it.
        staging.apply(&mut graph);
        assert_eq!(graph.definitions("a")[0].path, "a.rs");
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
    async fn a_format_change_forces_a_full_walk() {
        // Without this, an existing view keeps pre-upgrade extraction forever:
        // the incremental driver carries unchanged slices forward untouched, so
        // a parser improvement never reaches files that did not change (#223).
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "a.rs", "fn a() {}");
        git_init_commit(src.path());
        index(root.path(), src.path(), "r", "main").await.unwrap();

        // A second run would normally take the incremental path...
        let fmt = view_db_path(&root.path().join("graphs"), "r", "main").with_extension("fmt");
        assert_eq!(
            std::fs::read_to_string(&fmt).unwrap(),
            EXTRACTION_VERSION.to_string()
        );
        assert!(
            index(root.path(), src.path(), "r", "main")
                .await
                .unwrap()
                .incremental
        );

        // ...but not when the view was built by an older extraction format.
        std::fs::write(&fmt, "1").unwrap();
        let summary = index(root.path(), src.path(), "r", "main").await.unwrap();
        assert!(!summary.incremental, "a format change must rebuild in full");
        assert_eq!(
            std::fs::read_to_string(&fmt).unwrap(),
            EXTRACTION_VERSION.to_string(),
            "and must record the version it rebuilt with"
        );
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

    // ── index: view membership (#209) ────────────────────────────────────────

    #[tokio::test]
    async fn index_excludes_vendored_bundles_from_the_view() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "a.rs", "fn a() {}");
        write_file(src.path(), "mermaid.min.js", "var a=1,e=2,t=3;");

        let summary = index(root.path(), src.path(), "r", "main").await.unwrap();
        assert_eq!(summary.files, 1, "only the hand-written source");
        assert_eq!(summary.ignored.files, 1, "the minified bundle, reported");

        let graph =
            SqliteGraphStore::open(view_db_path(&root.path().join("graphs"), "r", "main")).unwrap();
        assert!(
            graph.all_symbols().iter().all(|s| s.path == "a.rs"),
            "no symbol may come from a vendored bundle"
        );
    }

    #[tokio::test]
    async fn incremental_reindex_prunes_paths_a_laxer_run_admitted() {
        // The upgrade path: a view indexed before the filter existed still holds
        // vendored bundles. They never change, so they never appear in the git
        // diff, and once a base commit is recorded there is no full walk — so
        // without pruning the carried-forward set they would persist forever.
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "a.rs", "fn a() {}");
        write_file(src.path(), "vendor.min.js", "var a=1,e=2;");

        // Index once with a filter that admits the bundle, standing in for the
        // pre-#209 behaviour.
        let lax = IndexFilter::new(&["vendor.min.js".to_string()]);
        let first = index_with(root.path(), src.path(), "r", "main", &lax)
            .await
            .unwrap();
        assert_eq!(first.files, 2, "the bundle is in the view to begin with");

        // Re-index with the default rules. The bundle is unchanged, so only the
        // carried-forward prune can remove it.
        let second = index_with(
            root.path(),
            src.path(),
            "r",
            "main",
            &IndexFilter::default(),
        )
        .await
        .unwrap();
        assert_eq!(second.deleted, 1, "the bundle is dropped from the view");
        assert_eq!(second.ignored.files, 1);

        let graph =
            SqliteGraphStore::open(view_db_path(&root.path().join("graphs"), "r", "main")).unwrap();
        assert!(
            graph.all_symbols().iter().all(|s| s.path == "a.rs"),
            "no vendored symbol may survive the re-index"
        );
    }

    #[tokio::test]
    async fn incremental_reindex_keeps_paths_the_filter_still_admits() {
        // The prune must not eat ordinary carried-forward files.
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "a.rs", "fn a() {}");
        write_file(src.path(), "b.rs", "fn b() {}");
        index(root.path(), src.path(), "r", "main").await.unwrap();

        let second = index(root.path(), src.path(), "r", "main").await.unwrap();
        assert_eq!(second.deleted, 0, "nothing legitimate is pruned");
        assert_eq!(second.ignored.files, 0);
    }

    #[tokio::test]
    async fn index_can_be_told_to_keep_a_vendored_path() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "a.rs", "fn a() {}");
        write_file(src.path(), "keep.min.js", "function f(){}");

        let filter = IndexFilter::new(&["keep.min.js".to_string()]);
        let summary = index_with(root.path(), src.path(), "r", "main", &filter)
            .await
            .unwrap();
        assert_eq!(summary.files, 2, "the override re-admits it");
        assert_eq!(summary.ignored.files, 0);
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

    // ── watch: debounce core (gonzalo#100) ──────────────────────────────────

    #[test]
    fn debouncer_not_due_without_events() {
        let d = Debouncer::new(Duration::from_millis(500));
        assert!(!d.is_pending());
        assert!(!d.is_due(Instant::now()));
    }

    #[test]
    fn debouncer_due_only_after_quiet_window() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(500));
        d.on_event(t0);
        assert!(d.is_pending());
        // Still inside the window → not due.
        assert!(!d.is_due(t0 + Duration::from_millis(499)));
        // Window elapsed → due.
        assert!(d.is_due(t0 + Duration::from_millis(500)));
    }

    #[test]
    fn debouncer_coalesces_a_burst() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(500));
        d.on_event(t0);
        d.on_event(t0 + Duration::from_millis(200)); // second edit resets the clock
        // 500ms after the *first* event is not enough — the burst is still hot.
        assert!(!d.is_due(t0 + Duration::from_millis(500)));
        // 500ms after the *last* event → due, and only once for the whole burst.
        assert!(d.is_due(t0 + Duration::from_millis(700)));
    }

    #[test]
    fn debouncer_clear_resets_pending() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(500));
        d.on_event(t0);
        d.clear();
        assert!(!d.is_pending());
        assert!(!d.is_due(t0 + Duration::from_secs(10)));
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
