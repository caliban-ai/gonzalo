//! Command implementations for the gonzalo admin CLI.

use anyhow::Result;
use gonzalo_core::{
    Body, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind, Revision, Store,
    segment,
};
use gonzalo_store_fs::FsStore;
use std::collections::BTreeMap;
use std::path::Path;

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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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
}
