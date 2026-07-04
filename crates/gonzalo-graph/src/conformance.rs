//! A reusable conformance suite every [`GraphStore`] impl must pass, so a
//! persistent backend (e.g. SQLite, ticket B) is provably equivalent to the
//! in-memory reference. Backend crates call [`run_graph_store_conformance`]
//! from their tests with a factory that returns a fresh, empty store.

use crate::{GraphStore, build_rust};
use std::collections::BTreeSet;

/// Run the full suite against stores produced by `make` (a fresh, empty
/// [`GraphStore`] per call).
pub fn run_graph_store_conformance<S: GraphStore>(make: impl Fn() -> S) {
    definitions_locate_by_path(&mut seeded(&make));
    symbols_in_file_filters_by_path(&mut seeded(&make));
    callers_callees_and_impact(&mut seeded(&make));
    references_to_reports_paths(&mut seeded(&make));
    reinsert_replaces_a_path(&mut seeded(&make));
    enumerates_all_symbols_and_references(&mut seeded(&make));
    empty_store_answers_are_empty(&mut make());
}

/// A store seeded with a small two-file call chain:
/// `leaf` (lib.rs) ← `mid` (lib.rs) ← `top` (main.rs).
fn seeded<S: GraphStore>(make: &impl Fn() -> S) -> S {
    let mut s = make();
    s.insert("lib.rs", build_rust("fn leaf() {}\nfn mid() { leaf(); }"));
    s.insert("main.rs", build_rust("fn top() { mid(); }"));
    s
}

fn definitions_locate_by_path<S: GraphStore>(s: &mut S) {
    let defs = s.definitions("leaf");
    assert_eq!(defs.len(), 1, "one definition of leaf");
    assert_eq!(defs[0].path, "lib.rs");
    assert_eq!(defs[0].item.name, "leaf");
    assert_eq!(s.definitions("top")[0].path, "main.rs");
    assert!(s.definitions("nonexistent").is_empty());
}

fn symbols_in_file_filters_by_path<S: GraphStore>(s: &mut S) {
    let lib: Vec<String> = s
        .symbols_in_file("lib.rs")
        .into_iter()
        .map(|sy| sy.name)
        .collect();
    assert!(lib.contains(&"leaf".to_string()));
    assert!(lib.contains(&"mid".to_string()));
    assert!(!lib.contains(&"top".to_string()));
    assert!(s.symbols_in_file("absent.rs").is_empty());
}

fn callers_callees_and_impact<S: GraphStore>(s: &mut S) {
    assert_eq!(s.callers_of("leaf"), vec!["mid".to_string()]);
    assert_eq!(s.callers_of("mid"), vec!["top".to_string()]);
    assert_eq!(s.callees("mid"), vec!["leaf".to_string()]);
    assert_eq!(s.callees("top"), vec!["mid".to_string()]);
    assert!(s.callees("leaf").is_empty());
    // Transitive caller closure, seed-excluded and sorted.
    assert_eq!(s.impact("leaf"), vec!["mid".to_string(), "top".to_string()]);
    assert_eq!(s.impact("mid"), vec!["top".to_string()]);
    assert!(s.impact("top").is_empty());
}

fn references_to_reports_paths<S: GraphStore>(s: &mut S) {
    let refs = s.references_to("mid");
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].path, "main.rs");
    assert_eq!(refs[0].item.from.as_deref(), Some("top"));
}

fn reinsert_replaces_a_path<S: GraphStore>(s: &mut S) {
    // Re-assembling the same path must not duplicate its symbols.
    s.insert("lib.rs", build_rust("fn leaf() {}\nfn mid() { leaf(); }"));
    assert_eq!(s.definitions("leaf").len(), 1);
    assert_eq!(s.callers_of("leaf"), vec!["mid".to_string()]);
}

fn enumerates_all_symbols_and_references<S: GraphStore>(s: &mut S) {
    let symbol_names: BTreeSet<String> = s.all_symbols().into_iter().map(|l| l.item.name).collect();
    assert_eq!(
        symbol_names,
        BTreeSet::from(["leaf".to_string(), "mid".to_string(), "top".to_string()])
    );
    // Every reference to `leaf` and `mid` appears exactly once in the full set.
    let refs = s.all_references();
    assert_eq!(refs.iter().filter(|r| r.item.name == "leaf").count(), 1);
    assert_eq!(refs.iter().filter(|r| r.item.name == "mid").count(), 1);
    assert!(
        s.all_symbols()
            .iter()
            .any(|l| l.item.name == "top" && l.path == "main.rs")
    );
}

fn empty_store_answers_are_empty<S: GraphStore>(s: &mut S) {
    assert!(s.definitions("anything").is_empty());
    assert!(s.callers_of("anything").is_empty());
    assert!(s.callees("anything").is_empty());
    assert!(s.impact("anything").is_empty());
    assert!(s.symbols_in_file("anything").is_empty());
    assert!(s.references_to("anything").is_empty());
    assert!(s.all_symbols().is_empty());
    assert!(s.all_references().is_empty());
}
