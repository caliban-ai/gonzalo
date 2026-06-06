//! Storage and structural queries over a [`CodeGraph`].

use crate::model::{CodeGraph, Reference, Symbol};

/// Structural queries over an accumulated code graph.
pub trait GraphStore: Send + Sync {
    /// Merge a parsed graph into the store.
    fn insert(&mut self, graph: CodeGraph);
    /// Symbols defined in `file`.
    fn symbols_in_file(&self, file: &str) -> Vec<Symbol>;
    /// Definitions matching `name` (there may be several, e.g. overloads in
    /// different modules — names are unresolved).
    fn definitions(&self, name: &str) -> Vec<Symbol>;
    /// References whose target name is `name`.
    fn references_to(&self, name: &str) -> Vec<Reference>;
    /// Distinct enclosing-function names that reference `name`.
    fn callers_of(&self, name: &str) -> Vec<String>;
}

/// An in-memory [`GraphStore`].
#[derive(Debug, Default)]
pub struct InMemoryGraphStore {
    graph: CodeGraph,
}

impl InMemoryGraphStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the accumulated graph (e.g. to serialize it into a record).
    pub fn graph(&self) -> &CodeGraph {
        &self.graph
    }
}

impl GraphStore for InMemoryGraphStore {
    fn insert(&mut self, graph: CodeGraph) {
        self.graph.extend(graph);
    }

    fn symbols_in_file(&self, file: &str) -> Vec<Symbol> {
        self.graph
            .symbols
            .iter()
            .filter(|s| s.file == file)
            .cloned()
            .collect()
    }

    fn definitions(&self, name: &str) -> Vec<Symbol> {
        self.graph
            .symbols
            .iter()
            .filter(|s| s.name == name)
            .cloned()
            .collect()
    }

    fn references_to(&self, name: &str) -> Vec<Reference> {
        self.graph
            .references
            .iter()
            .filter(|r| r.name == name)
            .cloned()
            .collect()
    }

    fn callers_of(&self, name: &str) -> Vec<String> {
        let mut callers: Vec<String> = self
            .graph
            .references
            .iter()
            .filter(|r| r.name == name)
            .filter_map(|r| r.from.clone())
            .collect();
        callers.sort();
        callers.dedup();
        callers
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
        s.insert(build_rust("lib.rs", SRC));
        s
    }

    #[test]
    fn definitions_and_symbols_in_file() {
        let s = store();
        assert_eq!(s.definitions("helper").len(), 1);
        assert!(s.symbols_in_file("lib.rs").iter().any(|sy| sy.name == "a"));
        assert!(s.symbols_in_file("other.rs").is_empty());
    }

    #[test]
    fn callers_of_dedups_and_sorts() {
        let s = store();
        assert_eq!(
            s.callers_of("helper"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn references_to_counts_all() {
        let s = store();
        assert_eq!(s.references_to("helper").len(), 2);
    }
}
