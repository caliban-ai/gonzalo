//! Assemble a view's manifest into a queryable graph (ADR 0012, ticket C1).
//!
//! The manifest (`path -> content_hash`) is the identity layer; the blob store
//! holds the path-agnostic slices. Assembly fetches each referenced slice and
//! inserts it under its manifest path, re-attaching the path the slice itself
//! does not carry.

use crate::{CodeGraph, GraphStore, InMemoryGraphStore};
use gonzalo_core::{BlobStore, CoreError, Manifest, Result};

/// Assemble `manifest` into an in-memory graph by fetching each slice from
/// `blobs` and inserting it under its path.
///
/// **Tolerates missing targets** (ADR 0012): a manifest entry whose blob is not
/// present is an honest dangling reference — skipped, not an error — so a view
/// mid-sync still assembles the slices it does have. A blob that exists but is
/// not a valid serialized slice *is* an error (corrupt store).
pub async fn assemble<B: BlobStore>(manifest: &Manifest, blobs: &B) -> Result<InMemoryGraphStore> {
    let mut store = InMemoryGraphStore::new();
    for (path, hash) in &manifest.entries {
        if let Some(bytes) = blobs.get_blob(hash).await? {
            let graph =
                CodeGraph::from_slice_bytes(&bytes).map_err(|e| CoreError::Serde(e.to_string()))?;
            store.insert(path, graph);
        }
    }
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_rust;
    use gonzalo_store_fs::FsStore;

    fn fresh_store() -> FsStore {
        let dir = tempfile::tempdir().expect("tempdir");
        FsStore::new(dir.keep())
    }

    /// Store a slice's bytes and return its content hash.
    async fn put_slice(blobs: &FsStore, src: &str) -> gonzalo_core::ContentHash {
        blobs
            .put_blob(&build_rust(src).to_slice_bytes())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn assembles_slices_under_their_manifest_paths() {
        let blobs = fresh_store();
        let lib = put_slice(&blobs, "fn helper() {}").await;
        let main = put_slice(&blobs, "fn main() { helper(); }").await;

        let mut manifest = Manifest::new();
        manifest.insert("src/lib.rs", lib);
        manifest.insert("src/main.rs", main);

        let graph = assemble(&manifest, &blobs).await.unwrap();

        // Definition resolves to the path the manifest placed it under.
        let defs = graph.definitions("helper");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].path, "src/lib.rs");
        // The cross-file call is present and attributed to the calling file.
        let callers = graph.callers_of("helper");
        assert_eq!(callers, vec!["main".to_string()]);
        assert!(
            graph
                .symbols_in_file("src/main.rs")
                .iter()
                .any(|s| s.name == "main")
        );
    }

    #[tokio::test]
    async fn tolerates_a_missing_slice_blob() {
        let blobs = fresh_store();
        let present = put_slice(&blobs, "fn present() {}").await;

        let mut manifest = Manifest::new();
        manifest.insert("present.rs", present);
        // A manifest entry whose slice was never stored (or already GC'd).
        manifest.insert("missing.rs", gonzalo_core::ContentHash::of(b"never stored"));

        let graph = assemble(&manifest, &blobs).await.unwrap();
        assert_eq!(graph.definitions("present").len(), 1);
        assert!(graph.symbols_in_file("missing.rs").is_empty());
    }

    #[tokio::test]
    async fn path_comes_from_manifest_so_identical_content_dedups() {
        let blobs = fresh_store();
        // The same file content assembled under two paths: one stored blob,
        // two manifest entries — the path is supplied at assembly, not baked in.
        let hash = put_slice(&blobs, "fn shared() {}").await;
        let same = put_slice(&blobs, "fn shared() {}").await;
        assert_eq!(hash, same, "identical content must dedup to one blob");

        let mut manifest = Manifest::new();
        manifest.insert("a.rs", hash.clone());
        manifest.insert("vendor/a.rs", hash);

        let graph = assemble(&manifest, &blobs).await.unwrap();
        let mut paths: Vec<String> = graph
            .definitions("shared")
            .into_iter()
            .map(|l| l.path)
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["a.rs".to_string(), "vendor/a.rs".to_string()]);
    }

    #[tokio::test]
    async fn empty_manifest_assembles_empty_graph() {
        let blobs = fresh_store();
        let graph = assemble(&Manifest::new(), &blobs).await.unwrap();
        assert!(graph.slices().is_empty());
    }
}
