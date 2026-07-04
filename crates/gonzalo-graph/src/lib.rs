//! Code-graph capability layer: parse source into a symbol/reference graph
//! and answer structural queries (`definitions`, `references_to`,
//! `callers_of`). Rust is the first supported language (tree-sitter-rust);
//! the model and store are language-agnostic so more grammars can be added.

pub mod assembly;
pub mod builder;
pub mod model;
pub mod resolve;
pub mod store;

pub use assembly::assemble;
pub use builder::{Language, build, build_rust};
pub use model::{CodeGraph, Located, Reference, Symbol, SymbolKind};
pub use resolve::{Resolution, ResolvedReference, resolve_references_to, resolved_callers_of};
pub use store::{GraphStore, InMemoryGraphStore};

#[cfg(feature = "conformance")]
pub mod conformance;
