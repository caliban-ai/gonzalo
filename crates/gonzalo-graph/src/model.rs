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
    /// The type or module this symbol is defined inside — `Foo` for a method in
    /// `impl Foo`, `Widget` for a method in `class Widget`, `util` for a
    /// function in an inline `mod util`.
    ///
    /// Without it `Foo::get` and `Bar::get` are one node, because a symbol's
    /// identity is its bare name. Paired with [`Reference::qualifier`] it lets
    /// the resolver keep two same-named methods apart with no type inference
    /// (#248). Omitted from the serialized slice when absent, so a file of
    /// plain free functions keeps the byte-identical slice — and therefore the
    /// same content hash — it had before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
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
    /// The path segment immediately before the callee name at the call site —
    /// `Language` for `Language::from_extension()`, `b` for `a::b::c()`.
    ///
    /// Recorded only for path-shaped calls. A method call's receiver is a
    /// *value*, not a type, so it is deliberately left `None`: `x` says nothing
    /// about which `helper` is meant, and storing it here would assert
    /// something the graph does not know (#248).
    ///
    /// Omitted from the serialized slice when absent, like [`kind`](Self::kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualifier: Option<String>,
    /// What [`from`](Self::from) names — a function, or a module-level binding.
    ///
    /// Before #268 `from` was always a function, and a call at module level
    /// simply had none. A great deal of modern TypeScript lives in module-level
    /// builder objects (a Zod schema, a tRPC router), so those calls reached
    /// neither `callers` nor `impact`.
    ///
    /// Attributing them to the binding widens what `from` means, so the scope
    /// travels *with* it rather than the meaning changing silently: "who calls
    /// this" and "which module-level declaration mentions this" are different
    /// questions and a consumer must be able to tell them apart. Defaults to
    /// [`Function`](FromScope::Function) and is omitted from the serialized
    /// slice then, so a file with no module-level attribution keeps a
    /// byte-identical slice.
    #[serde(default, skip_serializing_if = "FromScope::is_function")]
    pub from_scope: FromScope,
}

/// What a [`Reference`]'s `from` names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FromScope {
    /// A function: a declaration, a method, or a function bound to a name. The
    /// only kind before #268, and the default — so a slice written before it
    /// deserializes to exactly what it meant.
    #[default]
    Function,
    /// A module-level binding that is not a function — `const schema =
    /// z.object({..})`, a tRPC handler keyed in a router object. The call sits
    /// in that binding's initializer at module level (#268).
    ///
    /// Deliberately over-inclusive in the same way [`RefKind::Value`] is: a
    /// module-level `const rows = items.map(..)` attributes its lambda's calls
    /// to `rows`. At module level the alternative was no attribution at all,
    /// and this field is what lets a consumer filter rather than guess.
    Module,
}

impl FromScope {
    /// Whether this is the default, [`Function`](FromScope::Function) scope.
    pub fn is_function(&self) -> bool {
        matches!(self, Self::Function)
    }

    /// Lowercase name, matching the `snake_case` serde representation. The
    /// stored value in the persistent graph.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Module => "module",
        }
    }

    /// Parse from [`as_str`](FromScope::as_str). Anything unrecognized —
    /// including a row written before the column existed — reads as
    /// `Function`, the pre-existing behaviour.
    pub fn from_str_or_function(raw: &str) -> Self {
        match raw {
            "module" => Self::Module,
            _ => Self::Function,
        }
    }
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
    /// A name used as a *value* rather than called — `and_then(helper)`.
    ///
    /// Not a call edge, and deliberately kept out of `callers`/`callees` and the
    /// impact walk. It exists so `unreferenced` can tell a live callback from a
    /// dead function: a path expression records no call, so a function only ever
    /// passed as a value looked unused and was reported as deletable (#250).
    ///
    /// Over-inclusive by construction. Extraction is per-file, so it cannot tell
    /// an identifier naming a function from one naming a local; both are
    /// recorded. For `unreferenced` that errs towards not calling something
    /// dead, which is the safe direction.
    Value,
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
            Self::Value => "value",
        }
    }

    /// Parse from [`as_str`](RefKind::as_str). Anything unrecognized — including
    /// a row written before the column existed — reads as `Free`, the
    /// pre-existing behaviour.
    pub fn from_str_or_free(raw: &str) -> Self {
        match raw {
            "method" => Self::Method,
            "value" => Self::Value,
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
    /// Names this file brings into scope. Omitted from the serialized slice
    /// when empty, so a file with no imports keeps the byte-identical slice —
    /// and therefore the same content hash — it had before this existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<Import>,
}

/// A name brought into a file's scope, and the module path it came from.
///
/// Recorded as a **lexical scoping signal, not a resolved path** (#252).
/// `use crate::model::Symbol` says that in this file `Symbol` came from a module
/// called `model`, which is enough to prefer a definition under `model.rs` over
/// a same-named one elsewhere — without needing to know where a crate root is,
/// and so without extraction depending on any file but this one (ADR 0012).
///
/// Relative markers (`crate`, `self`, `super`, `.`, `..`) are stripped: they
/// position the path rather than naming a module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    /// The name as this file will write it — the alias when there is one.
    ///
    /// **Empty** when the import brings in a whole file rather than one name: a
    /// C/C++ `#include` names a path, not a symbol (#267). Test that with
    /// [`brings_whole_file`](Self::brings_whole_file) rather than comparing
    /// against the empty string, so the intent is visible at the call site.
    pub name: String,
    /// The module path segments before the name, outermost first.
    pub path: Vec<String>,
    /// How many dots a relative import leads with; 0 when absolute.
    ///
    /// A Python relative import is the one module path resolvable with no
    /// project root at all: `from ..pkg import X` means "my parent package,
    /// then `pkg`", which is arithmetic on the referencing file's own path.
    /// Dropping the dots threw that away and left the module to be matched as a
    /// free-floating suffix against every candidate (#261).
    ///
    /// Omitted from the serialized slice when zero, so a file with only
    /// absolute imports keeps a byte-identical slice.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub depth: usize,
    pub line: usize,
}

impl Import {
    /// Whether this import brings in a whole file rather than one name.
    ///
    /// A `#include` names a path — every declaration in that file becomes
    /// visible — so it has no single name to key on the way every other import
    /// does. Resolution matches it by path instead (#267).
    pub fn brings_whole_file(&self) -> bool {
        self.name.is_empty()
    }
}

/// Whether a count is zero, for `skip_serializing_if`.
fn is_zero(n: &usize) -> bool {
    *n == 0
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
