//! The code-graph data model. Serializable so a graph can be persisted as a
//! gonzalo record and shared/synced like any other data.

use serde::{Deserialize, Serialize};

/// What kind of Rust item a symbol is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// A class (Python `class`, and other languages that have classes).
    Class,
    /// An interface (TypeScript `interface`, and similar constructs).
    Interface,
}

/// A defined symbol with its in-file location (1-based line numbers).
///
/// **Path-agnostic** (ADR 0012): a symbol carries no file path, so the same
/// file content produces byte-identical slices regardless of where it lives,
/// and content-addressed storage dedups them across paths/worktrees. The path
/// is supplied at assembly from the manifest — see [`Located`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: usize,
    pub end_line: usize,
}

/// A name-based reference (e.g. a call) from within `from` (the enclosing
/// function symbol, if any) to `name`. References are unresolved: they match
/// by name, not by a resolved definition. This is a heuristic call graph,
/// suitable for navigation; true name resolution is a later milestone.
///
/// Path-agnostic like [`Symbol`]; the path comes from assembly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub name: String,
    pub from: Option<String>,
    pub line: usize,
}

/// A query result carried with the assembly path it was found under. The path
/// is not stored in the slice ([`Symbol`]/[`Reference`] are path-agnostic); it
/// is re-attached at assembly from the manifest, so navigation still resolves
/// to a concrete file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Located<T> {
    pub path: String,
    pub item: T,
}

/// A code graph: the symbols defined and references found in a single file's
/// slice. Path-agnostic; a whole view is assembled from many of these keyed by
/// path in a [`GraphStore`](crate::GraphStore).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeGraph {
    pub symbols: Vec<Symbol>,
    pub references: Vec<Reference>,
}

impl CodeGraph {
    /// Serialize this slice to its content-addressed blob bytes (ADR 0012).
    /// Byte-stable for equal content, since the model carries no path.
    pub fn to_slice_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("CodeGraph serializes")
    }

    /// Deserialize a slice from its blob bytes.
    pub fn from_slice_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}
