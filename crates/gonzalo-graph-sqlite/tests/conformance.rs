//! The SQLite backend must answer identically to the in-memory reference, and
//! must actually persist a view across reopens (ticket B).

use gonzalo_graph::conformance::run_graph_store_conformance;
use gonzalo_graph::{GraphStore, InMemoryGraphStore, build_rust};
use gonzalo_graph_sqlite::SqliteGraphStore;

#[test]
fn in_memory_reference_passes_conformance() {
    // Sanity: the reference backend passes the same suite the SQLite one must.
    run_graph_store_conformance(InMemoryGraphStore::new);
}

#[test]
fn sqlite_passes_graph_store_conformance() {
    run_graph_store_conformance(|| SqliteGraphStore::open_in_memory().unwrap());
}

#[test]
fn sqlite_persists_a_view_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.db");

    {
        let mut store = SqliteGraphStore::open(&db).unwrap();
        store.insert(
            "lib.rs",
            build_rust("fn helper() {}\nfn main() { helper(); }"),
        );
    } // drop closes the connection

    // Reopen the same file: the view is still queryable without re-indexing.
    let store = SqliteGraphStore::open(&db).unwrap();
    let defs = store.definitions("helper");
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].path, "lib.rs");
    assert_eq!(store.callers_of("helper"), vec!["main".to_string()]);
}
