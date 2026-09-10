//! Code-graph capability layer: parse source into a symbol/reference graph
//! and answer structural queries (`definitions`, `references_to`,
//! `callers_of`). Rust is the first supported language (tree-sitter-rust);
//! the model and store are language-agnostic so more grammars can be added.

/// Version of the slice/graph extraction format.
///
/// Bumped whenever a change alters what the parser records for unchanged source
/// — a new [`Reference`] field, a new edge rule, a grammar fix. An indexer that
/// recorded a different version must do a full walk rather than an incremental
/// one: carrying slices forward untouched is exactly what would otherwise leave
/// a view permanently half-upgraded.
///
/// History: 1 = calls in macro arguments (#216); 2 = call-shape on references
/// (#223); 3 = call-site qualifiers and definition owners (#248); 4 = functions
/// named as values (#250); 5 = syntactic receiver types on method calls (#251);
/// 6 = per-file imports (#252); 7 = Java/Kotlin imports (#260); 8 = JS/TS
/// functions bound through a higher-order call, and object properties (#257).
pub const EXTRACTION_VERSION: u32 = 8;

pub mod assembly;
pub mod builder;
pub mod diff;
pub mod model;
pub mod resolve;
pub mod store;

pub use assembly::assemble;
pub use builder::{Language, build, build_rust};
pub use diff::{GraphDiff, diff};
pub use model::{
    CodeGraph, FileSummary, Import, Located, Page, RankedSymbol, Ranking, RefKind, Reference,
    Symbol, SymbolFilter, SymbolKind, ViewOverview,
};
pub use resolve::{
    ImpactNode, ImpactReport, Resolution, ResolvedReference, resolve_references_to,
    resolved_callers_of, resolved_impact,
};
pub use store::{GraphStore, InMemoryGraphStore};

#[cfg(feature = "conformance")]
pub mod conformance;
