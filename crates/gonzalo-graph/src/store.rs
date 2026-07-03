//! Storage and structural queries over an assembled view's slices.
//!
//! Slices are path-agnostic (ADR 0012); the store keys each slice by the path
//! it was assembled under, so location-bearing queries return [`Located`]
//! results while the stored [`Symbol`]/[`Reference`] stay path-free.

use crate::model::{CodeGraph, Located, Reference, Symbol};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Structural queries over an assembled view (a set of path-keyed slices).
pub trait GraphStore: Send + Sync {
    /// Insert (or replace) the slice assembled at `path`. Re-inserting the same
    /// path overwrites — slices are content-addressed and write-if-absent, never
    /// appended, so this cannot duplicate a file's symbols.
    fn insert(&mut self, path: &str, graph: CodeGraph);
    /// Symbols defined in the slice at `path`.
    fn symbols_in_file(&self, path: &str) -> Vec<Symbol>;
    /// Definitions matching `name`, each with the path it was found under (there
    /// may be several — names are unresolved).
    fn definitions(&self, name: &str) -> Vec<Located<Symbol>>;
    /// References whose target name is `name`, each with its path.
    fn references_to(&self, name: &str) -> Vec<Located<Reference>>;
    /// Distinct enclosing-function names that reference `name`.
    fn callers_of(&self, name: &str) -> Vec<String>;
    /// Distinct names referenced from within `name` (the inverse of
    /// [`callers_of`](Self::callers_of)), sorted.
    fn callees(&self, name: &str) -> Vec<String>;

    /// The transitive closure of callers: every symbol that could be affected if
    /// `name` changes, reached by walking [`callers_of`](Self::callers_of)
    /// breadth-first. Runs server-side (never ships the whole graph); the seed
    /// `name` is excluded, and cycles terminate via a visited set. Returned
    /// sorted.
    fn impact(&self, name: &str) -> Vec<String> {
        let mut visited = BTreeSet::from([name.to_string()]);
        let mut queue: VecDeque<String> = VecDeque::new();
        for caller in self.callers_of(name) {
            if visited.insert(caller.clone()) {
                queue.push_back(caller);
            }
        }
        while let Some(current) = queue.pop_front() {
            for caller in self.callers_of(&current) {
                if visited.insert(caller.clone()) {
                    queue.push_back(caller);
                }
            }
        }
        visited.remove(name);
        visited.into_iter().collect()
    }
}

/// An in-memory [`GraphStore`], one slice per path.
#[derive(Debug, Default)]
pub struct InMemoryGraphStore {
    slices: BTreeMap<String, CodeGraph>,
}

impl InMemoryGraphStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The assembled slices, keyed by path.
    pub fn slices(&self) -> &BTreeMap<String, CodeGraph> {
        &self.slices
    }
}

impl GraphStore for InMemoryGraphStore {
    fn insert(&mut self, path: &str, graph: CodeGraph) {
        self.slices.insert(path.to_string(), graph);
    }

    fn symbols_in_file(&self, path: &str) -> Vec<Symbol> {
        self.slices
            .get(path)
            .map(|g| g.symbols.clone())
            .unwrap_or_default()
    }

    fn definitions(&self, name: &str) -> Vec<Located<Symbol>> {
        self.slices
            .iter()
            .flat_map(|(path, g)| {
                g.symbols
                    .iter()
                    .filter(|s| s.name == name)
                    .map(move |s| Located {
                        path: path.clone(),
                        item: s.clone(),
                    })
            })
            .collect()
    }

    fn references_to(&self, name: &str) -> Vec<Located<Reference>> {
        self.slices
            .iter()
            .flat_map(|(path, g)| {
                g.references
                    .iter()
                    .filter(|r| r.name == name)
                    .map(move |r| Located {
                        path: path.clone(),
                        item: r.clone(),
                    })
            })
            .collect()
    }

    fn callers_of(&self, name: &str) -> Vec<String> {
        let mut callers: Vec<String> = self
            .slices
            .values()
            .flat_map(|g| g.references.iter())
            .filter(|r| r.name == name)
            .filter_map(|r| r.from.clone())
            .collect();
        callers.sort();
        callers.dedup();
        callers
    }

    fn callees(&self, name: &str) -> Vec<String> {
        let mut callees: Vec<String> = self
            .slices
            .values()
            .flat_map(|g| g.references.iter())
            .filter(|r| r.from.as_deref() == Some(name))
            .map(|r| r.name.clone())
            .collect();
        callees.sort();
        callees.dedup();
        callees
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::build_rust;

    const SRC: &str = r#"
fn helper() {}
fn a() { helper(); }
fn b() { helper(); }
"#;

    fn store() -> InMemoryGraphStore {
        let mut s = InMemoryGraphStore::new();
        s.insert("lib.rs", build_rust(SRC));
        s
    }

    #[test]
    fn definitions_carry_their_path() {
        let s = store();
        let defs = s.definitions("helper");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].path, "lib.rs");
        assert_eq!(defs[0].item.name, "helper");
    }

    #[test]
    fn symbols_in_file_filters_by_path() {
        let s = store();
        assert!(s.symbols_in_file("lib.rs").iter().any(|sy| sy.name == "a"));
        assert!(s.symbols_in_file("other.rs").is_empty());
    }

    #[test]
    fn definitions_span_multiple_paths() {
        let mut s = InMemoryGraphStore::new();
        s.insert("a.rs", build_rust("fn dup() {}"));
        s.insert("b.rs", build_rust("fn dup() {}"));
        let mut paths: Vec<String> = s.definitions("dup").into_iter().map(|l| l.path).collect();
        paths.sort();
        assert_eq!(paths, vec!["a.rs".to_string(), "b.rs".to_string()]);
    }

    #[test]
    fn reinserting_same_path_replaces_not_appends() {
        let mut s = store();
        // Re-assembling the same path must not duplicate its symbols.
        s.insert("lib.rs", build_rust(SRC));
        assert_eq!(s.definitions("helper").len(), 1);
    }

    #[test]
    fn callers_of_dedups_and_sorts_across_slices() {
        let s = store();
        assert_eq!(
            s.callers_of("helper"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn references_to_counts_all_with_paths() {
        let s = store();
        let refs = s.references_to("helper");
        assert_eq!(refs.len(), 2);
        assert!(refs.iter().all(|r| r.path == "lib.rs"));
    }

    const CHAIN: &str = r#"
fn leaf() {}
fn mid() { leaf(); }
fn top() { mid(); }
fn other() { leaf(); }
"#;

    fn chain() -> InMemoryGraphStore {
        let mut s = InMemoryGraphStore::new();
        s.insert("chain.rs", build_rust(CHAIN));
        s
    }

    #[test]
    fn callees_lists_names_called_from_a_function() {
        let s = chain();
        assert_eq!(s.callees("mid"), vec!["leaf".to_string()]);
        assert_eq!(s.callees("top"), vec!["mid".to_string()]);
        assert!(s.callees("leaf").is_empty());
    }

    #[test]
    fn callees_dedups_repeated_calls() {
        let mut s = InMemoryGraphStore::new();
        s.insert("d.rs", build_rust("fn f() { g(); g(); h(); }"));
        assert_eq!(s.callees("f"), vec!["g".to_string(), "h".to_string()]);
    }

    #[test]
    fn impact_is_the_transitive_caller_closure() {
        let s = chain();
        // Everything transitively affected if `leaf` changes: its callers mid &
        // other, and mid's caller top.
        assert_eq!(
            s.impact("leaf"),
            vec!["mid".to_string(), "other".to_string(), "top".to_string()]
        );
        assert_eq!(s.impact("mid"), vec!["top".to_string()]);
        assert!(s.impact("top").is_empty());
    }

    #[test]
    fn impact_excludes_the_seed_and_survives_cycles() {
        let mut s = InMemoryGraphStore::new();
        s.insert("cyc.rs", build_rust("fn a() { b(); } fn b() { a(); }"));
        // a<->b mutually recurse; impact must terminate and not report the seed.
        assert_eq!(s.impact("a"), vec!["b".to_string()]);
        assert_eq!(s.impact("b"), vec!["a".to_string()]);
    }
}
