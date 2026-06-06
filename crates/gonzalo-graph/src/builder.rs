//! Build a [`CodeGraph`] from Rust source using tree-sitter.

use crate::model::{CodeGraph, Reference, Symbol, SymbolKind};
use tree_sitter::{Node, Parser};

/// Parse `src` (the contents of `file`) and extract its symbols and
/// name-based references.
pub fn build_rust(file: &str, src: &str) -> CodeGraph {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("load rust grammar");
    let Some(tree) = parser.parse(src, None) else {
        return CodeGraph::default();
    };
    let mut graph = CodeGraph::default();
    let bytes = src.as_bytes();
    walk(tree.root_node(), bytes, file, None, &mut graph);
    graph
}

fn node_text<'a>(node: Node<'_>, bytes: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(bytes).ok()
}

fn name_field(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|n| node_text(n, bytes))
        .map(str::to_string)
}

fn item_kind(kind: &str) -> Option<SymbolKind> {
    Some(match kind {
        "function_item" => SymbolKind::Function,
        "struct_item" => SymbolKind::Struct,
        "enum_item" => SymbolKind::Enum,
        "trait_item" => SymbolKind::Trait,
        "impl_item" => SymbolKind::Impl,
        "mod_item" => SymbolKind::Module,
        "const_item" => SymbolKind::Const,
        "static_item" => SymbolKind::Static,
        "type_item" => SymbolKind::TypeAlias,
        _ => return None,
    })
}

/// The display name of an item node. `impl_item` has no `name` field, so we
/// use the text of its `type` field (e.g. `Foo` in `impl Foo`).
fn item_name(node: Node<'_>, kind: SymbolKind, bytes: &[u8]) -> Option<String> {
    match kind {
        SymbolKind::Impl => node
            .child_by_field_name("type")
            .and_then(|n| node_text(n, bytes))
            .map(str::to_string),
        _ => name_field(node, bytes),
    }
}

/// The called name of a `call_expression`'s `function` field.
fn call_name(func: Node<'_>, bytes: &[u8]) -> Option<String> {
    match func.kind() {
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
        // generic function turbofish etc.: fall back to the whole text
        _ => node_text(func, bytes).map(str::to_string),
    }
}

fn walk(node: Node<'_>, bytes: &[u8], file: &str, current_fn: Option<&str>, graph: &mut CodeGraph) {
    let mut enclosing = current_fn.map(str::to_string);

    if let Some(kind) = item_kind(node.kind())
        && let Some(name) = item_name(node, kind, bytes)
    {
        graph.symbols.push(Symbol {
            name: name.clone(),
            kind,
            file: file.to_string(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
        });
        if kind == SymbolKind::Function {
            enclosing = Some(name);
        }
    }

    if node.kind() == "call_expression"
        && let Some(func) = node.child_by_field_name("function")
        && let Some(name) = call_name(func, bytes)
    {
        graph.references.push(Reference {
            name,
            from: enclosing.clone(),
            file: file.to_string(),
            line: node.start_position().row + 1,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, bytes, file, enclosing.as_deref(), graph);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
struct Widget { n: u32 }

fn helper(x: u32) -> u32 { x + 1 }

fn main() {
    let w = Widget { n: 1 };
    let y = helper(w.n);
    println!("{y}");
}
"#;

    #[test]
    fn extracts_definitions() {
        let g = build_rust("lib.rs", SRC);
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
    fn records_call_with_enclosing_fn() {
        let g = build_rust("lib.rs", SRC);
        let call = g
            .references
            .iter()
            .find(|r| r.name == "helper")
            .expect("helper call recorded");
        assert_eq!(call.from.as_deref(), Some("main"));
    }

    #[test]
    fn symbol_lines_are_one_based() {
        let g = build_rust("lib.rs", SRC);
        let main = g.symbols.iter().find(|s| s.name == "main").unwrap();
        assert!(main.start_line >= 1 && main.end_line >= main.start_line);
    }
}
