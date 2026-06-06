//! Code-graph capability layer: parse source into a symbol/reference graph
//! and answer structural queries (`definitions`, `references_to`,
//! `callers_of`). Rust is the first supported language (tree-sitter-rust);
//! the model and store are language-agnostic so more grammars can be added.

pub mod builder;
pub mod model;
pub mod store;

pub use builder::build_rust;
pub use model::{CodeGraph, Reference, Symbol, SymbolKind};
pub use store::{GraphStore, InMemoryGraphStore};
