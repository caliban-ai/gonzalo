//! Build a [`CodeGraph`] from source using tree-sitter. Parsing is
//! language-parameterized ([`Language`]); Rust, Python, JavaScript,
//! TypeScript/TSX, Go, Java, C#, C, C++, Ruby, PHP, Bash, and Kotlin are
//! supported, and a new grammar is a matter of adding its node-kind mappings.

use crate::model::{CodeGraph, Reference, Symbol, SymbolKind};
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser};

/// A source language the graph builder understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    /// TypeScript with JSX (`.tsx`).
    Tsx,
    Go,
    Java,
    CSharp,
    C,
    Cpp,
    Ruby,
    Php,
    Bash,
    Kotlin,
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
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            "cs" => Some(Self::CSharp),
            "c" | "h" => Some(Self::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some(Self::Cpp),
            "rb" => Some(Self::Ruby),
            "php" => Some(Self::Php),
            "sh" | "bash" => Some(Self::Bash),
            "kt" | "kts" => Some(Self::Kotlin),
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
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
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
            // Go names a type on the `type_spec`, but struct vs interface is
            // determined by its inner `type` node — so the symbol is defined at
            // the `struct_type`/`interface_type` node, and `item_name` reaches
            // back to the enclosing `type_spec` for the name. `const`/`var` specs
            // are best-effort (first name of a possibly multi-name spec).
            Self::Go => match node_kind {
                "function_declaration" | "method_declaration" => Some(SymbolKind::Function),
                "struct_type" => Some(SymbolKind::Struct),
                "interface_type" => Some(SymbolKind::Interface),
                "const_spec" => Some(SymbolKind::Const),
                "var_spec" => Some(SymbolKind::Static),
                _ => None,
            },
            Self::Java => match node_kind {
                "class_declaration" => Some(SymbolKind::Class),
                "interface_declaration" => Some(SymbolKind::Interface),
                "enum_declaration" => Some(SymbolKind::Enum),
                "method_declaration" | "constructor_declaration" => Some(SymbolKind::Function),
                _ => None,
            },
            Self::CSharp => match node_kind {
                "class_declaration" => Some(SymbolKind::Class),
                "interface_declaration" => Some(SymbolKind::Interface),
                "struct_declaration" => Some(SymbolKind::Struct),
                "enum_declaration" => Some(SymbolKind::Enum),
                "method_declaration" | "constructor_declaration" => Some(SymbolKind::Function),
                _ => None,
            },
            Self::C => c_item_kind(node_kind),
            // C++ is a superset of C's declarations.
            Self::Cpp => c_item_kind(node_kind).or(match node_kind {
                "class_specifier" => Some(SymbolKind::Class),
                "namespace_definition" => Some(SymbolKind::Module),
                _ => None,
            }),
            Self::Ruby => match node_kind {
                "method" | "singleton_method" => Some(SymbolKind::Function),
                "class" => Some(SymbolKind::Class),
                "module" => Some(SymbolKind::Module),
                _ => None,
            },
            Self::Php => match node_kind {
                "function_definition" | "method_declaration" => Some(SymbolKind::Function),
                "class_declaration" => Some(SymbolKind::Class),
                "interface_declaration" => Some(SymbolKind::Interface),
                "trait_declaration" => Some(SymbolKind::Trait),
                "enum_declaration" => Some(SymbolKind::Enum),
                _ => None,
            },
            // Bash has only functions.
            Self::Bash => match node_kind {
                "function_definition" => Some(SymbolKind::Function),
                _ => None,
            },
            // Kotlin `object` (a named singleton) and `interface` both surface as
            // class-like type declarations; interfaces are `class_declaration`
            // with an `interface` keyword, so they read as Class here.
            Self::Kotlin => match node_kind {
                "function_declaration" => Some(SymbolKind::Function),
                "class_declaration" | "object_declaration" => Some(SymbolKind::Class),
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
            // Go `struct_type`/`interface_type` carry no name; the name lives on
            // the enclosing `type_spec`. Anonymous types (no `type_spec` parent
            // with a name) yield `None` and are skipped.
            (Self::Go, SymbolKind::Struct | SymbolKind::Interface) => node
                .parent()
                .and_then(|p| p.child_by_field_name("name"))
                .and_then(|n| node_text(n, bytes))
                .map(str::to_string),
            // C/C++ name a function or typedef through nested `declarator` nodes,
            // not a flat `name` field. Struct/enum/class/namespace do use `name`.
            (Self::C | Self::Cpp, SymbolKind::Function | SymbolKind::TypeAlias) => {
                c_declarator_name(node, bytes)
            }
            _ => name_field(node, bytes),
        }
    }

    /// Whether `node_kind` is a call expression for this language.
    fn is_call(self, node_kind: &str) -> bool {
        match self {
            Self::Rust
            | Self::JavaScript
            | Self::TypeScript
            | Self::Tsx
            | Self::Go
            | Self::C
            | Self::Cpp => node_kind == "call_expression",
            Self::Python => node_kind == "call",
            Self::Java => node_kind == "method_invocation",
            Self::CSharp => node_kind == "invocation_expression",
            Self::Ruby => node_kind == "call",
            Self::Php => node_kind == "function_call_expression",
            // Bash "calls" are commands (`helper arg`).
            Self::Bash => node_kind == "command",
            Self::Kotlin => node_kind == "call_expression",
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
            Self::Go => match func.kind() {
                "identifier" => node_text(func, bytes).map(str::to_string),
                // pkg.Func(...) / x.Method(...) -> the selector's `field`.
                "selector_expression" => func
                    .child_by_field_name("field")
                    .and_then(|n| node_text(n, bytes))
                    .map(str::to_string),
                _ => node_text(func, bytes).map(str::to_string),
            },
            Self::CSharp => match func.kind() {
                "identifier" => node_text(func, bytes).map(str::to_string),
                // obj.Method(...) -> the member access's `name` field.
                "member_access_expression" => func
                    .child_by_field_name("name")
                    .and_then(|n| node_text(n, bytes))
                    .map(str::to_string),
                _ => node_text(func, bytes).map(str::to_string),
            },
            Self::C | Self::Cpp => match func.kind() {
                "identifier" => node_text(func, bytes).map(str::to_string),
                // x.m(...) / x->m(...) -> the field expression's `field`.
                "field_expression" => func
                    .child_by_field_name("field")
                    .and_then(|n| node_text(n, bytes))
                    .map(str::to_string),
                // C++ `Ns::func(...)` -> the qualified id's `name` (last segment).
                "qualified_identifier" => func
                    .child_by_field_name("name")
                    .and_then(|n| node_text(n, bytes))
                    .map(str::to_string),
                _ => node_text(func, bytes).map(str::to_string),
            },
            // PHP `function_call_expression` holds the callee in a `function`
            // field — a `name` (or `qualified_name`) node; take its text.
            Self::Php => node_text(func, bytes).map(str::to_string),
            // Java, Ruby, Bash, and Kotlin route through `callee_name` (their
            // callee is a dedicated field/child on the call node, not a nested
            // `function` node); these arms exist only to keep the match exhaustive.
            Self::Java | Self::Ruby | Self::Bash | Self::Kotlin => {
                node_text(func, bytes).map(str::to_string)
            }
        }
    }

    /// The called name from a call node. Most languages hold the callee in a
    /// `function` field (dispatched by [`call_name`]); Java's `method_invocation`
    /// instead carries the method name directly in its `name` field.
    fn callee_name(self, call: Node<'_>, bytes: &[u8]) -> Option<String> {
        match self {
            // Java's `method_invocation` and Bash's `command` carry the callee in
            // a `name` field (an identifier / a `command_name` node).
            Self::Java | Self::Bash => call
                .child_by_field_name("name")
                .and_then(|n| node_text(n, bytes))
                .map(str::to_string),
            // Ruby's `call` names the callee in a `method` field.
            Self::Ruby => call
                .child_by_field_name("method")
                .and_then(|n| node_text(n, bytes))
                .map(str::to_string),
            // Kotlin `call_expression` has no field; the callee is its first named
            // child — an `identifier` (`helper(..)`) or a `navigation_expression`
            // (`a.b.method(..)`), whose trailing identifier is the invoked member.
            Self::Kotlin => call
                .named_child(0)
                .and_then(|callee| last_identifier(callee, bytes)),
            _ => call
                .child_by_field_name("function")
                .and_then(|func| self.call_name(func, bytes)),
        }
    }
}

/// The last `identifier` in `node`'s subtree (depth-first). For a Kotlin callee
/// that is a bare `identifier` this is the node itself; for a
/// `navigation_expression` (`a.b.method`) it is the trailing member name.
fn last_identifier(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let mut result = if matches!(node.kind(), "identifier" | "simple_identifier") {
        node_text(node, bytes).map(str::to_string)
    } else {
        None
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(name) = last_identifier(child, bytes) {
            result = Some(name);
        }
    }
    result
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

/// C declaration node kinds shared by C and C++ (C++ adds classes/namespaces).
fn c_item_kind(node_kind: &str) -> Option<SymbolKind> {
    match node_kind {
        "function_definition" => Some(SymbolKind::Function),
        "struct_specifier" => Some(SymbolKind::Struct),
        "enum_specifier" => Some(SymbolKind::Enum),
        "type_definition" => Some(SymbolKind::TypeAlias),
        _ => None,
    }
}

/// Extract the identifier from a C/C++ `declarator` chain: descend the nested
/// `declarator` field (through pointer/function/parenthesized declarators) until
/// an identifier-like leaf is reached. Anonymous declarators yield `None`.
fn c_declarator_name(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let mut n = node;
    loop {
        if matches!(
            n.kind(),
            "identifier" | "field_identifier" | "type_identifier" | "qualified_identifier"
        ) {
            return node_text(n, bytes).map(str::to_string);
        }
        n = n.child_by_field_name("declarator")?;
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
        && let Some(name) = language.callee_name(node, bytes)
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
        assert_eq!(Language::from_extension("go"), Some(Language::Go));
        assert_eq!(Language::from_extension("java"), Some(Language::Java));
        assert_eq!(Language::from_extension("cs"), Some(Language::CSharp));
        assert_eq!(Language::from_extension("c"), Some(Language::C));
        assert_eq!(Language::from_extension("h"), Some(Language::C));
        assert_eq!(Language::from_extension("cpp"), Some(Language::Cpp));
        assert_eq!(Language::from_extension("cc"), Some(Language::Cpp));
        assert_eq!(Language::from_extension("cxx"), Some(Language::Cpp));
        assert_eq!(Language::from_extension("hpp"), Some(Language::Cpp));
        assert_eq!(Language::from_extension("hh"), Some(Language::Cpp));
        assert_eq!(Language::from_extension("rb"), Some(Language::Ruby));
        assert_eq!(Language::from_extension("php"), Some(Language::Php));
        assert_eq!(Language::from_extension("sh"), Some(Language::Bash));
        assert_eq!(Language::from_extension("bash"), Some(Language::Bash));
        assert_eq!(Language::from_extension("kt"), Some(Language::Kotlin));
        assert_eq!(Language::from_extension("kts"), Some(Language::Kotlin));
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

    const GO_SRC: &str = r#"
package main

type Widget struct { n int }

type Shape interface { Area() int }

const Limit = 10

var counter = 0

func helper(x int) int { return x + 1 }

func (w Widget) Area() int { return helper(w.n) }

func main() {
	w := Widget{n: 1}
	_ = w.Area()
	_ = helper(2)
}
"#;

    #[test]
    fn go_extracts_definitions() {
        let g = build(Language::Go, GO_SRC);
        let names: Vec<(&str, SymbolKind)> = g
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind))
            .collect();
        assert!(names.contains(&("Widget", SymbolKind::Struct)));
        assert!(names.contains(&("Shape", SymbolKind::Interface)));
        assert!(names.contains(&("helper", SymbolKind::Function)));
        assert!(names.contains(&("Area", SymbolKind::Function)));
        assert!(names.contains(&("main", SymbolKind::Function)));
        assert!(names.contains(&("Limit", SymbolKind::Const)));
        assert!(names.contains(&("counter", SymbolKind::Static)));
    }

    #[test]
    fn go_records_calls_including_methods() {
        let g = build(Language::Go, GO_SRC);
        // Plain call `helper(...)` from inside the `Area` method.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("Area")),
            "helper call from Area"
        );
        // Method call `w.Area()` -> selector-expression field `Area`, from `main`.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "Area" && r.from.as_deref() == Some("main")),
            "w.Area() call from main"
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

    const JAVA_SRC: &str = r#"
interface Shape { int area(); }

enum Color { RED, GREEN }

class Widget {
    int n;
    Widget(int n) { this.n = n; }
    int area() { return helper(this.n); }
}

class Main {
    static int helper(int x) { return x + 1; }
    static void main(String[] args) {
        Widget w = new Widget(1);
        w.area();
    }
}
"#;

    #[test]
    fn java_extracts_definitions() {
        let g = build(Language::Java, JAVA_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("Shape"), Some(SymbolKind::Interface));
        assert_eq!(named("Color"), Some(SymbolKind::Enum));
        assert_eq!(named("Widget"), Some(SymbolKind::Class));
        assert_eq!(named("area"), Some(SymbolKind::Function));
        assert_eq!(named("helper"), Some(SymbolKind::Function));
        // Constructor is recorded as a Function named for its class.
        assert!(
            g.symbols
                .iter()
                .any(|s| s.name == "Widget" && s.kind == SymbolKind::Function)
        );
    }

    #[test]
    fn java_records_calls_with_enclosing_fn() {
        let g = build(Language::Java, JAVA_SRC);
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("area")),
            "helper() call from area"
        );
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "area" && r.from.as_deref() == Some("main")),
            "w.area() call from main"
        );
    }

    const CS_SRC: &str = r#"
interface IShape { int Area(); }
enum Color { Red, Green }
struct Point { public int X; }

class Widget {
    int n;
    public Widget(int n) { this.n = n; }
    public int Area() { return Helper(this.n); }
}

class Program {
    static int Helper(int x) { return x + 1; }
    static void Main() {
        var w = new Widget(1);
        w.Area();
    }
}
"#;

    #[test]
    fn csharp_extracts_definitions() {
        let g = build(Language::CSharp, CS_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("IShape"), Some(SymbolKind::Interface));
        assert_eq!(named("Color"), Some(SymbolKind::Enum));
        assert_eq!(named("Point"), Some(SymbolKind::Struct));
        assert_eq!(named("Widget"), Some(SymbolKind::Class));
        assert_eq!(named("Area"), Some(SymbolKind::Function));
        assert_eq!(named("Helper"), Some(SymbolKind::Function));
    }

    #[test]
    fn csharp_records_calls_with_enclosing_fn() {
        let g = build(Language::CSharp, CS_SRC);
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "Helper" && r.from.as_deref() == Some("Area")),
            "Helper() call from Area"
        );
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "Area" && r.from.as_deref() == Some("Main")),
            "w.Area() call from Main"
        );
    }

    const C_SRC: &str = r#"
struct Widget { int n; };

typedef int Id;

enum Color { RED, GREEN };

int helper(int x) { return x + 1; }

int main(void) {
    int y = helper(2);
    return y;
}
"#;

    #[test]
    fn c_extracts_definitions() {
        let g = build(Language::C, C_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("Widget"), Some(SymbolKind::Struct));
        assert_eq!(named("Id"), Some(SymbolKind::TypeAlias));
        assert_eq!(named("Color"), Some(SymbolKind::Enum));
        assert_eq!(named("helper"), Some(SymbolKind::Function));
        assert_eq!(named("main"), Some(SymbolKind::Function));
    }

    #[test]
    fn c_records_call_with_enclosing_fn() {
        let g = build(Language::C, C_SRC);
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("main")),
            "helper() call from main"
        );
    }

    const CPP_SRC: &str = r#"
namespace geo {

class Widget {
public:
    int n;
    int area() { return helper(this->n); }
};

int helper(int x) { return x + 1; }

}

int main() {
    geo::Widget w;
    return w.area();
}
"#;

    #[test]
    fn cpp_extracts_definitions() {
        let g = build(Language::Cpp, CPP_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("geo"), Some(SymbolKind::Module));
        assert_eq!(named("Widget"), Some(SymbolKind::Class));
        assert_eq!(named("area"), Some(SymbolKind::Function));
        assert_eq!(named("helper"), Some(SymbolKind::Function));
        assert_eq!(named("main"), Some(SymbolKind::Function));
    }

    #[test]
    fn cpp_records_calls_including_methods() {
        let g = build(Language::Cpp, CPP_SRC);
        // this->helper(...) -> field_expression `field`, from `area`.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("area")),
            "helper() call from area"
        );
        // w.area() -> field_expression `field`, from `main`.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "area" && r.from.as_deref() == Some("main")),
            "w.area() call from main"
        );
    }

    const RUBY_SRC: &str = r#"
class Widget
  def area
    helper(1)
  end
end

module Util
end

def helper(x)
  x + 1
end

def main
  helper(2)
end
"#;

    #[test]
    fn ruby_extracts_definitions() {
        let g = build(Language::Ruby, RUBY_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("Widget"), Some(SymbolKind::Class));
        assert_eq!(named("Util"), Some(SymbolKind::Module));
        assert_eq!(named("area"), Some(SymbolKind::Function));
        assert_eq!(named("helper"), Some(SymbolKind::Function));
        assert_eq!(named("main"), Some(SymbolKind::Function));
    }

    #[test]
    fn ruby_records_call_with_enclosing_fn() {
        let g = build(Language::Ruby, RUBY_SRC);
        // `helper(2)` is called from the `main` method (callee is the `method` field).
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("main")),
            "helper call from main"
        );
    }

    const PHP_SRC: &str = r#"<?php
class Widget {
    function area() {
        return helper(1);
    }
}

interface Shape {}

trait Named {}

function helper($x) {
    return $x + 1;
}

function main() {
    return helper(2);
}
"#;

    #[test]
    fn php_extracts_definitions() {
        let g = build(Language::Php, PHP_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("Widget"), Some(SymbolKind::Class));
        assert_eq!(named("Shape"), Some(SymbolKind::Interface));
        assert_eq!(named("Named"), Some(SymbolKind::Trait));
        assert_eq!(named("area"), Some(SymbolKind::Function));
        assert_eq!(named("helper"), Some(SymbolKind::Function));
        assert_eq!(named("main"), Some(SymbolKind::Function));
    }

    #[test]
    fn php_records_call_with_enclosing_fn() {
        let g = build(Language::Php, PHP_SRC);
        // `helper(2)` -> function_call_expression `function` field, from `main`.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("main")),
            "helper() call from main"
        );
    }

    const BASH_SRC: &str = r#"
helper() {
  echo "$1"
}

main() {
  helper hello
}
"#;

    #[test]
    fn bash_extracts_definitions() {
        let g = build(Language::Bash, BASH_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("helper"), Some(SymbolKind::Function));
        assert_eq!(named("main"), Some(SymbolKind::Function));
    }

    #[test]
    fn bash_records_call_with_enclosing_fn() {
        let g = build(Language::Bash, BASH_SRC);
        // `helper hello` is a command whose `name` field is the callee, from `main`.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("main")),
            "helper command from main"
        );
    }

    const KOTLIN_SRC: &str = r#"
class Widget {
    fun area(): Int {
        return helper(1)
    }
}

object Config

fun helper(x: Int): Int {
    return x + 1
}

fun main() {
    helper(2)
}
"#;

    #[test]
    fn kotlin_extracts_definitions() {
        let g = build(Language::Kotlin, KOTLIN_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("Widget"), Some(SymbolKind::Class));
        assert_eq!(named("Config"), Some(SymbolKind::Class)); // `object` singleton
        assert_eq!(named("area"), Some(SymbolKind::Function));
        assert_eq!(named("helper"), Some(SymbolKind::Function));
        assert_eq!(named("main"), Some(SymbolKind::Function));
    }

    #[test]
    fn kotlin_records_call_with_enclosing_fn() {
        let g = build(Language::Kotlin, KOTLIN_SRC);
        // `helper(2)` -> call_expression whose first child is an `identifier`,
        // from `main`.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("main")),
            "helper() call from main"
        );
    }
}
