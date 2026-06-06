//! The code-graph data model. Serializable so a graph can be persisted as a
//! gonzalo record and shared/synced like any other data.

use serde::{Deserialize, Serialize};

/// What kind of Rust item a symbol is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    Const,
    Static,
    TypeAlias,
}

/// A defined symbol with its source location (1-based line numbers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// A name-based reference (e.g. a call) from within `from` (the enclosing
/// function symbol, if any) to `name`. References are unresolved: they match
/// by name, not by a resolved definition. This is a heuristic call graph,
/// suitable for navigation; true name resolution is a later milestone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub name: String,
    pub from: Option<String>,
    pub file: String,
    pub line: usize,
}

/// A code graph: the symbols defined and references found in one or more files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeGraph {
    pub symbols: Vec<Symbol>,
    pub references: Vec<Reference>,
}

impl CodeGraph {
    /// Merge another graph into this one.
    pub fn extend(&mut self, other: CodeGraph) {
        self.symbols.extend(other.symbols);
        self.references.extend(other.references);
    }
}
