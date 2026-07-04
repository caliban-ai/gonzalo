//! Build a [`CodeGraph`] from source using tree-sitter. Parsing is
//! language-parameterized ([`Language`]); Rust and Python are supported, and a
//! new grammar is a matter of adding its node-kind mappings.

use crate::model::{CodeGraph, Reference, Symbol, SymbolKind};
use tree_sitter::{Node, Parser};

/// A source language the graph builder understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
}

impl Language {
    /// The [`Language`] for a file extension (without the dot), or `None` if
    /// unsupported.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            _ => None,
        }
    }

    fn ts_language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
        }
    }

    /// Map a node kind to the symbol it defines, if any.
    fn item_kind(self, node_kind: &str) -> Option<SymbolKind> {
        match self {
            Self::Rust => match node_kind {
                "function_item" => Some(SymbolKind::Function),
                "struct_item" => Some(SymbolKind::Struct),
                "enum_item" => Some(SymbolKind::Enum),
                "trait_item" => Some(SymbolKind::Trait),
                "impl_item" => Some(SymbolKind::Impl),
                "mod_item" => Some(SymbolKind::Module),
                "const_item" => Some(SymbolKind::Const),
                "static_item" => Some(SymbolKind::Static),
                "type_item" => Some(SymbolKind::TypeAlias),
                _ => None,
            },
            Self::Python => match node_kind {
                "function_definition" => Some(SymbolKind::Function),
                "class_definition" => Some(SymbolKind::Class),
                _ => None,
            },
        }
    }

    /// The display name of an item node.
    fn item_name(self, node: Node<'_>, kind: SymbolKind, bytes: &[u8]) -> Option<String> {
        match (self, kind) {
            // Rust `impl` has no `name` field; use its `type` (e.g. `Foo` in `impl Foo`).
            (Self::Rust, SymbolKind::Impl) => node
                .child_by_field_name("type")
                .and_then(|n| node_text(n, bytes))
                .map(str::to_string),
            _ => name_field(node, bytes),
        }
    }

    /// Whether `node_kind` is a call expression for this language.
    fn is_call(self, node_kind: &str) -> bool {
        match self {
            Self::Rust => node_kind == "call_expression",
            Self::Python => node_kind == "call",
        }
    }

    /// The called name from a call node's `function` field.
    fn call_name(self, func: Node<'_>, bytes: &[u8]) -> Option<String> {
        match self {
            Self::Rust => match func.kind() {
                "identifier" => node_text(func, bytes).map(str::to_string),
                // a::b::c -> the `name` field (last segment)
                "scoped_identifier" => func
                    .child_by_field_name("name")
                    .and_then(|n| node_text(n, bytes))
                    .map(str::to_string),
                // x.method(...) -> the `field` field
                "field_expression" => func
                    .child_by_field_name("field")
                    .and_then(|n| node_text(n, bytes))
                    .map(str::to_string),
                _ => node_text(func, bytes).map(str::to_string),
            },
            Self::Python => match func.kind() {
                "identifier" => node_text(func, bytes).map(str::to_string),
                // obj.method(...) -> the `attribute` field (method name)
                "attribute" => func
                    .child_by_field_name("attribute")
                    .and_then(|n| node_text(n, bytes))
                    .map(str::to_string),
                _ => node_text(func, bytes).map(str::to_string),
            },
        }
    }
}

/// Parse `src` as `language` into a **path-agnostic** slice: its symbols and
/// name-based references, with no file path (ADR 0012). The path is supplied
/// later at assembly from the manifest.
pub fn build(language: Language, src: &str) -> CodeGraph {
    let mut parser = Parser::new();
    if parser.set_language(&language.ts_language()).is_err() {
        return CodeGraph::default();
    }
    let Some(tree) = parser.parse(src, None) else {
        return CodeGraph::default();
    };
    let mut graph = CodeGraph::default();
    walk(language, tree.root_node(), src.as_bytes(), None, &mut graph);
    graph
}

/// Parse Rust source. Back-compatible shorthand for `build(Language::Rust, src)`.
pub fn build_rust(src: &str) -> CodeGraph {
    build(Language::Rust, src)
}

fn node_text<'a>(node: Node<'_>, bytes: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(bytes).ok()
}

fn name_field(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|n| node_text(n, bytes))
        .map(str::to_string)
}

fn walk(
    language: Language,
    node: Node<'_>,
    bytes: &[u8],
    current_fn: Option<&str>,
    graph: &mut CodeGraph,
) {
    let mut enclosing = current_fn.map(str::to_string);

    if let Some(kind) = language.item_kind(node.kind())
        && let Some(name) = language.item_name(node, kind, bytes)
    {
        graph.symbols.push(Symbol {
            name: name.clone(),
            kind,
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
        });
        if kind == SymbolKind::Function {
            enclosing = Some(name);
        }
    }

    if language.is_call(node.kind())
        && let Some(func) = node.child_by_field_name("function")
        && let Some(name) = language.call_name(func, bytes)
    {
        graph.references.push(Reference {
            name,
            from: enclosing.clone(),
            line: node.start_position().row + 1,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(language, child, bytes, enclosing.as_deref(), graph);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_SRC: &str = r#"
struct Widget { n: u32 }

fn helper(x: u32) -> u32 { x + 1 }

fn main() {
    let w = Widget { n: 1 };
    let y = helper(w.n);
    println!("{y}");
}
"#;

    #[test]
    fn rust_extracts_definitions() {
        let g = build_rust(RUST_SRC);
        let names: Vec<(&str, SymbolKind)> = g
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind))
            .collect();
        assert!(names.contains(&("Widget", SymbolKind::Struct)));
        assert!(names.contains(&("helper", SymbolKind::Function)));
        assert!(names.contains(&("main", SymbolKind::Function)));
    }

    #[test]
    fn rust_records_call_with_enclosing_fn() {
        let g = build_rust(RUST_SRC);
        let call = g
            .references
            .iter()
            .find(|r| r.name == "helper")
            .expect("helper call recorded");
        assert_eq!(call.from.as_deref(), Some("main"));
    }

    #[test]
    fn rust_symbol_lines_are_one_based() {
        let g = build_rust(RUST_SRC);
        let main = g.symbols.iter().find(|s| s.name == "main").unwrap();
        assert!(main.start_line >= 1 && main.end_line >= main.start_line);
    }

    const PY_SRC: &str = r#"
class Widget:
    def area(self):
        return helper(self.n)

def helper(x):
    return x + 1

def main():
    w = Widget()
    print(w.area())
"#;

    #[test]
    fn python_extracts_functions_and_classes() {
        let g = build(Language::Python, PY_SRC);
        let names: Vec<(&str, SymbolKind)> = g
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind))
            .collect();
        assert!(names.contains(&("Widget", SymbolKind::Class)));
        assert!(names.contains(&("helper", SymbolKind::Function)));
        assert!(names.contains(&("area", SymbolKind::Function)));
    }

    #[test]
    fn python_records_calls_including_methods() {
        let g = build(Language::Python, PY_SRC);
        // Plain call `helper(...)` from inside `area`.
        let helper_call = g
            .references
            .iter()
            .find(|r| r.name == "helper")
            .expect("helper call");
        assert_eq!(helper_call.from.as_deref(), Some("area"));
        // Method call `w.area()` from inside `main` -> the attribute name `area`.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "area" && r.from.as_deref() == Some("main"))
        );
    }

    #[test]
    fn language_from_extension() {
        assert_eq!(Language::from_extension("rs"), Some(Language::Rust));
        assert_eq!(Language::from_extension("py"), Some(Language::Python));
        assert_eq!(Language::from_extension("txt"), None);
    }
}
