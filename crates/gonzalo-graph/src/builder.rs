//! Build a [`CodeGraph`] from source using tree-sitter. Parsing is
//! language-parameterized ([`Language`]); Rust, Python, JavaScript, and
//! TypeScript/TSX are supported, and a new grammar is a matter of adding its
//! node-kind mappings.

use crate::model::{CodeGraph, Reference, Symbol, SymbolKind};
use tree_sitter::{Node, Parser};

/// A source language the graph builder understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    /// TypeScript with JSX (`.tsx`).
    Tsx,
}

impl Language {
    /// The [`Language`] for a file extension (without the dot), or `None` if
    /// unsupported.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            _ => None,
        }
    }

    fn ts_language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
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
            Self::JavaScript => js_item_kind(node_kind),
            // TypeScript/TSX are a superset of JavaScript's declarations.
            Self::TypeScript | Self::Tsx => js_item_kind(node_kind).or(match node_kind {
                "interface_declaration" => Some(SymbolKind::Interface),
                "type_alias_declaration" => Some(SymbolKind::TypeAlias),
                "enum_declaration" => Some(SymbolKind::Enum),
                _ => None,
            }),
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
            Self::Rust | Self::JavaScript | Self::TypeScript | Self::Tsx => {
                node_kind == "call_expression"
            }
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
            Self::JavaScript | Self::TypeScript | Self::Tsx => match func.kind() {
                "identifier" => node_text(func, bytes).map(str::to_string),
                // obj.method(...) -> the member expression's `property` field
                "member_expression" => func
                    .child_by_field_name("property")
                    .and_then(|n| node_text(n, bytes))
                    .map(str::to_string),
                _ => node_text(func, bytes).map(str::to_string),
            },
        }
    }
}

/// JavaScript declaration node kinds shared by JS and TS/TSX.
fn js_item_kind(node_kind: &str) -> Option<SymbolKind> {
    match node_kind {
        "function_declaration" | "generator_function_declaration" | "method_definition" => {
            Some(SymbolKind::Function)
        }
        "class_declaration" | "abstract_class_declaration" => Some(SymbolKind::Class),
        _ => None,
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
        assert_eq!(Language::from_extension("js"), Some(Language::JavaScript));
        assert_eq!(Language::from_extension("jsx"), Some(Language::JavaScript));
        assert_eq!(Language::from_extension("ts"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("tsx"), Some(Language::Tsx));
        assert_eq!(Language::from_extension("txt"), None);
    }

    const JS_SRC: &str = r#"
class Widget {
  area() {
    return helper(this.n);
  }
}
function helper(x) {
  return x + 1;
}
function main() {
  const w = new Widget();
  console.log(w.area());
}
"#;

    #[test]
    fn javascript_extracts_symbols_and_calls() {
        let g = build(Language::JavaScript, JS_SRC);
        let names: Vec<(&str, SymbolKind)> = g
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind))
            .collect();
        assert!(names.contains(&("Widget", SymbolKind::Class)));
        assert!(names.contains(&("area", SymbolKind::Function)));
        assert!(names.contains(&("helper", SymbolKind::Function)));
        assert!(names.contains(&("main", SymbolKind::Function)));

        // `helper(...)` called from inside `area`.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("area"))
        );
        // Method call `w.area()` -> member-expression property `area`, from `main`.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "area" && r.from.as_deref() == Some("main"))
        );
    }

    const TS_SRC: &str = r#"
interface Shape { area(): number; }
type Id = string;
enum Color { Red, Green }

class Circle implements Shape {
  area(): number { return compute(this.r); }
}

function compute(r: number): number { return r * r; }
"#;

    #[test]
    fn typescript_extracts_ts_specific_kinds() {
        let g = build(Language::TypeScript, TS_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("Shape"), Some(SymbolKind::Interface));
        assert_eq!(named("Id"), Some(SymbolKind::TypeAlias));
        assert_eq!(named("Color"), Some(SymbolKind::Enum));
        assert_eq!(named("Circle"), Some(SymbolKind::Class));
        assert_eq!(named("compute"), Some(SymbolKind::Function));

        assert!(
            g.references
                .iter()
                .any(|r| r.name == "compute" && r.from.as_deref() == Some("area"))
        );
    }

    #[test]
    fn tsx_parses_with_jsx() {
        // The TSX grammar must accept JSX syntax that plain TS would reject.
        let src = r#"
function App(): JSX.Element {
  return greet();
}
function greet() { return <div>hi</div>; }
"#;
        let g = build(Language::Tsx, src);
        assert!(g.symbols.iter().any(|s| s.name == "App"));
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "greet" && r.from.as_deref() == Some("App"))
        );
    }
}
