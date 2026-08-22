//! The code-graph data model. Serializable so a graph can be persisted as a
//! gonzalo record and shared/synced like any other data.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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

impl SymbolKind {
    /// Lowercase name, used as a stable key when bucketing symbols by kind.
    /// Matches the `snake_case` serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Module => "module",
            Self::Const => "const",
            Self::Static => "static",
            Self::TypeAlias => "type_alias",
            Self::Class => "class",
            Self::Interface => "interface",
        }
    }
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
    /// How the callee was written at the call site. Defaults to
    /// [`RefKind::Free`] and is omitted from the serialized slice when free, so
    /// a file of plain calls keeps the byte-identical slice — and therefore the
    /// same content hash — it had before this field existed.
    #[serde(default, skip_serializing_if = "RefKind::is_free")]
    pub kind: RefKind,
}

/// The syntactic shape of a call site.
///
/// A name alone cannot distinguish `chain()` from `x.chain()`, and conflating
/// them makes the resolver attribute a std or dependency method to a same-named
/// free function that happens to be the only one in the view (#223). Recording
/// the shape keeps that judgement possible at resolution time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    /// A plain call — `foo()` — or a path call such as `a::b::foo()`.
    #[default]
    Free,
    /// A call through a receiver whose type is unknown — `x.foo()`. The callee
    /// belongs to whatever `x` is, which the graph does not know, so it may well
    /// be defined outside the view entirely.
    Method,
}

impl RefKind {
    /// Whether this is the default, [`Free`](RefKind::Free) shape.
    pub fn is_free(&self) -> bool {
        matches!(self, Self::Free)
    }

    /// Lowercase name, matching the `snake_case` serde representation. Used as
    /// the stored value in the persistent graph.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Method => "method",
        }
    }

    /// Parse from [`as_str`](RefKind::as_str). Anything unrecognized — including
    /// a row written before the column existed — reads as `Free`, the
    /// pre-existing behaviour.
    pub fn from_str_or_free(raw: &str) -> Self {
        match raw {
            "method" => Self::Method,
            _ => Self::Free,
        }
    }
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

/// A file and the number of symbols defined in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSummary {
    pub path: String,
    pub symbols: usize,
}

/// The aggregate shape of a whole view — what is here, rather than facts about
/// one symbol. `by_kind` and `by_language` are keyed by the lowercase names from
/// [`SymbolKind::as_str`] and [`Language::as_str`](crate::Language::as_str);
/// symbols in files with an unrecognized extension bucket under `"unknown"`, so
/// `by_language` always sums to `symbols`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewOverview {
    /// Distinct paths contributing symbols or references.
    pub files: usize,
    pub symbols: usize,
    pub references: usize,
    pub by_kind: BTreeMap<String, usize>,
    pub by_language: BTreeMap<String, usize>,
    /// Files with the most symbols, descending. Bounded by the caller's limit;
    /// `files` above is the untruncated count.
    pub largest_files: Vec<FileSummary>,
}

/// A symbol name ranked by some score, with the paths that define it. `paths`
/// is empty when the name is referenced but never defined in this view (a call
/// into a dependency, or a name the parser saw but no slice declares).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedSymbol {
    pub name: String,
    pub score: usize,
    pub paths: Vec<String>,
}

/// What [`GraphStore::top`](crate::GraphStore::top) ranks by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ranking {
    /// Number of references to the name — how heavily it is called.
    FanIn,
    /// Number of distinct names called from within it.
    FanOut,
    /// Number of definitions of the name. A score above 1 means the name is
    /// ambiguous, which is what makes name-matched traversal unreliable.
    Definitions,
}

/// A conjunctive filter for [`GraphStore::list`](crate::GraphStore::list) — every
/// set field must match. All fields unset matches every symbol.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SymbolFilter {
    pub path_prefix: Option<String>,
    pub kind: Option<SymbolKind>,
    pub name_contains: Option<String>,
}

impl SymbolFilter {
    /// Restrict to symbols whose path starts with `prefix` (scopes to a crate
    /// or directory).
    #[must_use]
    pub fn path_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.path_prefix = Some(prefix.into());
        self
    }

    /// Restrict to one [`SymbolKind`].
    #[must_use]
    pub fn kind(mut self, kind: SymbolKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// Restrict to symbols whose name contains `needle`.
    #[must_use]
    pub fn name_contains(mut self, needle: impl Into<String>) -> Self {
        self.name_contains = Some(needle.into());
        self
    }

    /// Whether `located` satisfies every set field.
    pub fn matches(&self, located: &Located<Symbol>) -> bool {
        self.path_prefix
            .as_ref()
            .is_none_or(|p| located.path.starts_with(p.as_str()))
            && self.kind.is_none_or(|k| located.item.kind == k)
            && self
                .name_contains
                .as_ref()
                .is_none_or(|n| located.item.name.contains(n.as_str()))
    }
}

/// A bounded slice of a larger result set. `total` is the untruncated match
/// count, so a caller can always tell what it did not see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub truncated: bool,
}

impl<T> Page<T> {
    /// Take at most `limit` of `items`, recording the pre-truncation total.
    pub fn new(items: Vec<T>, limit: usize) -> Self {
        let total = items.len();
        let mut items = items;
        items.truncate(limit);
        Self {
            truncated: items.len() < total,
            items,
            total,
        }
    }
}
