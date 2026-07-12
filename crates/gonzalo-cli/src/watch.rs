//! Filesystem-watching driver for `gonzalo index --watch` (gonzalo#100).
//!
//! This is the I/O boundary: it bridges the [`notify`] OS watcher and Tokio
//! timers to the deterministic, unit-tested [`Debouncer`](crate::Debouncer) and
//! the set-reconciling [`index`](crate::index). A burst of edits is coalesced
//! into a single incremental re-index, and a slower periodic tick runs a full
//! reconcile so any event the OS watcher dropped is still converged (safe
//! because reconciliation is a pure set difference — a missed event is corrected
//! at the next pass, never lost).
//!
//! The event-loop glue here has no deterministic unit test (it owns real OS
//! notifications, wall-clock timers, and a Ctrl-C signal), so it is excluded
//! from the coverage gate via `scripts/coverage.sh`'s `IGNORE_REGEX`, exactly
//! like the daemon/worker entrypoints. The logic worth testing — debounce
//! coalescing — lives in [`Debouncer`](crate::Debouncer), which is covered.

use crate::{Debouncer, index_with_gc};
use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use std::path::Path;
use std::time::{Duration, Instant};

/// Timing knobs for [`watch`].
#[derive(Clone, Copy, Debug)]
pub struct WatchConfig {
    /// Quiet period after the last change before an incremental re-index fires,
    /// so a burst of edits coalesces into one pass.
    pub debounce: Duration,
    /// Interval between full reconciles that self-heal any missed events.
    pub full_reconcile: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(500),
            full_reconcile: Duration::from_secs(300),
        }
    }
}

/// Watch `src` and keep the `(repo, view)` code-graph view in sync until
/// Ctrl-C. Runs one index up front, then re-indexes on a debounced burst of
/// filesystem changes and on a periodic full-reconcile tick. Long-running:
/// returns `Ok(())` on graceful shutdown.
///
/// When `gc` is set, every index (the initial prime, each debounced re-index,
/// and each periodic reconcile) sweeps orphaned slices across all live views —
/// so `--gc` is honored under `--watch` rather than silently dropped (#157).
pub async fn watch(
    root: &Path,
    src: &Path,
    repo: &str,
    view: &str,
    config: WatchConfig,
    gc: bool,
) -> Result<()> {
    // Prime the view before watching, so a fresh run is immediately queryable.
    let (summary, swept) = index_with_gc(root, src, repo, view, gc).await?;
    eprintln!(
        "gonzalo watch: initial index ({} files, {} added, {} modified, {} deleted{})",
        summary.files,
        summary.added,
        summary.modified,
        summary.deleted,
        gc_note(&swept),
    );

    // Bridge notify's callback thread to the async loop over an unbounded
    // channel — we only care that *something* changed, not the exact paths
    // (index() re-derives the changed set itself).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && is_content_change(&event)
        {
            let _ = tx.send(());
        }
    })
    .context("creating filesystem watcher")?;
    watcher
        .watch(src, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", src.display()))?;

    let mut debouncer = Debouncer::new(config.debounce);
    // Poll the debouncer at its own resolution; fire once the burst settles.
    let mut debounce_tick = tokio::time::interval(config.debounce);
    let mut reconcile_tick = tokio::time::interval(config.full_reconcile);
    // interval fires immediately on first poll; skip that leading tick so the
    // periodic reconcile doesn't double the initial index.
    reconcile_tick.tick().await;

    eprintln!("gonzalo watch: watching {} (Ctrl-C to stop)", src.display());
    loop {
        tokio::select! {
            Some(()) = rx.recv() => {
                debouncer.on_event(Instant::now());
            }
            _ = debounce_tick.tick() => {
                if debouncer.is_due(Instant::now()) {
                    debouncer.clear();
                    reindex(root, src, repo, view, gc, "incremental").await;
                }
            }
            _ = reconcile_tick.tick() => {
                debouncer.clear(); // a full pass subsumes any pending change
                reindex(root, src, repo, view, gc, "full reconcile").await;
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("gonzalo watch: shutting down");
                break;
            }
        }
    }
    Ok(())
}

/// Run one re-index, logging the outcome. When `gc` is set the run also sweeps
/// orphaned slices. A failure is logged, not fatal — the watcher keeps running
/// so a transient error (e.g. a half-written file) is corrected on the next
/// event or reconcile.
async fn reindex(root: &Path, src: &Path, repo: &str, view: &str, gc: bool, reason: &str) {
    match index_with_gc(root, src, repo, view, gc).await {
        Ok((s, swept)) => eprintln!(
            "gonzalo watch: re-indexed ({reason}): {} added, {} modified, {} deleted{}",
            s.added,
            s.modified,
            s.deleted,
            gc_note(&swept),
        ),
        Err(e) => eprintln!("gonzalo watch: re-index failed ({reason}): {e:#}"),
    }
}

/// Human-readable `", gc freed N"` suffix when a sweep ran, else empty.
fn gc_note(swept: &Option<crate::GcSummary>) -> String {
    swept
        .as_ref()
        .map(|g| format!(", gc freed {}", g.freed))
        .unwrap_or_default()
}

/// Whether a notify event represents a content change worth re-indexing
/// (create/modify/remove/rename), ignoring pure metadata/access events.
fn is_content_change(event: &notify::Event) -> bool {
    use notify::EventKind;
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{gc, index};
    use tempfile::TempDir;

    /// #157: the `gc` flag threaded into the watch loop is honored — a reindex
    /// with `gc = true` sweeps orphaned slices (the OS-event/timer glue around
    /// this call is untestable, so we drive `reindex` directly).
    #[tokio::test]
    async fn reindex_sweeps_orphans_when_gc_is_set() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.rs"), "fn a() {}").unwrap();
        index(root.path(), src.path(), "r", "main").await.unwrap();

        // Change the file so the pre-edit slice is orphaned, then reindex under
        // the watch loop's helper with gc enabled.
        std::fs::write(src.path().join("a.rs"), "fn a() { b(); }").unwrap();
        reindex(root.path(), src.path(), "r", "main", true, "test").await;

        // The orphan was already swept during the reindex, so an explicit gc
        // finds nothing left to free.
        assert_eq!(gc(root.path()).await.unwrap().freed, 0);
    }

    /// Counterpart: without the flag, the reindex leaves the orphan behind.
    #[tokio::test]
    async fn reindex_leaves_orphans_when_gc_is_unset() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.rs"), "fn a() {}").unwrap();
        index(root.path(), src.path(), "r", "main").await.unwrap();

        std::fs::write(src.path().join("a.rs"), "fn a() { b(); }").unwrap();
        reindex(root.path(), src.path(), "r", "main", false, "test").await;

        // The orphan survived: an explicit gc still has one to free.
        assert_eq!(gc(root.path()).await.unwrap().freed, 1);
    }
}
