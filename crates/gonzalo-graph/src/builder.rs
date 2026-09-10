//! Build a [`CodeGraph`] from source using tree-sitter. Parsing is
//! language-parameterized ([`Language`]); Rust, Python, JavaScript,
//! TypeScript/TSX, Go, Java, C#, C, C++, Ruby, PHP, Bash, Kotlin, Swift, Lua,
//! Scala, and Elixir are supported, and a new grammar is a matter of adding its
//! node-kind mappings.

use crate::model::{CodeGraph, Import, RefKind, Reference, Symbol, SymbolKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
    Swift,
    Lua,
    Scala,
    Elixir,
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
            "c" => Some(Self::C),
            // `.h` is the conventional header extension for C++ as much as for
            // C, and the C grammar mis-parses C++ rather than failing: `enum
            // class Color` recorded `Color` as a function and invented a symbol
            // named `class`. C++ is a near superset and the two agree on plain
            // C — pinned by
            // [`a_pure_c_header_extracts_the_same_symbols_under_either_grammar`]
            // — so a header goes through the wider grammar (#266).
            "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some(Self::Cpp),
            "rb" => Some(Self::Ruby),
            "php" => Some(Self::Php),
            "sh" | "bash" => Some(Self::Bash),
            "kt" | "kts" => Some(Self::Kotlin),
            "swift" => Some(Self::Swift),
            "lua" => Some(Self::Lua),
            "scala" | "sc" => Some(Self::Scala),
            "ex" | "exs" => Some(Self::Elixir),
            _ => None,
        }
    }

    /// Lowercase name, used as a stable key when bucketing symbols by language.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Go => "go",
            Self::Java => "java",
            Self::CSharp => "csharp",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::Bash => "bash",
            Self::Kotlin => "kotlin",
            Self::Swift => "swift",
            Self::Lua => "lua",
            Self::Scala => "scala",
            Self::Elixir => "elixir",
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
            Self::Swift => tree_sitter_swift::LANGUAGE.into(),
            Self::Lua => tree_sitter_lua::LANGUAGE.into(),
            Self::Scala => tree_sitter_scala::LANGUAGE.into(),
            Self::Elixir => tree_sitter_elixir::LANGUAGE.into(),
        }
    }

    /// Map a node to the symbol it defines, if any. Takes the whole node (not
    /// just its kind) because some languages need to inspect children — e.g. a
    /// JS `variable_declarator` is only a function when its value is an
    /// arrow/function expression, and Swift/Kotlin distinguish struct/enum/
    /// interface by a keyword child.
    fn item_kind(self, node: Node<'_>, bytes: &[u8]) -> Option<SymbolKind> {
        let node_kind = node.kind();
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
            Self::JavaScript => js_item_kind(node, bytes),
            // TypeScript/TSX are a superset of JavaScript's declarations.
            Self::TypeScript | Self::Tsx => js_item_kind(node, bytes).or(match node_kind {
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
            // Kotlin `class_declaration` covers both `class` and `interface`
            // (distinguished by a leading keyword child); `object` (a named
            // singleton) is its own `object_declaration` node and reads as Class.
            Self::Kotlin => match node_kind {
                "function_declaration" => Some(SymbolKind::Function),
                "class_declaration" => Some(kotlin_class_kind(node)),
                "object_declaration" => Some(SymbolKind::Class),
                _ => None,
            },
            // Swift `class_declaration` covers class/struct/enum/actor,
            // distinguished by a `declaration_kind` keyword child; `protocol`
            // maps to Interface.
            Self::Swift => match node_kind {
                "function_declaration" => Some(SymbolKind::Function),
                "class_declaration" => Some(swift_type_kind(node)),
                "protocol_declaration" => Some(SymbolKind::Interface),
                _ => None,
            },
            // Lua has only functions (named `function_declaration`; anonymous
            // `function_definition` carries no name and is skipped).
            Self::Lua => match node_kind {
                "function_declaration" => Some(SymbolKind::Function),
                _ => None,
            },
            // Scala `object` (a named singleton) surfaces as a class-like type.
            Self::Scala => match node_kind {
                "function_definition" | "function_declaration" => Some(SymbolKind::Function),
                "class_definition" | "object_definition" => Some(SymbolKind::Class),
                "trait_definition" => Some(SymbolKind::Trait),
                "enum_definition" => Some(SymbolKind::Enum),
                _ => None,
            },
            // Elixir is homoiconic: `def`/`defp`/`defmacro`/`defmacrop` and
            // `defmodule` all parse as ordinary `call` nodes distinguished by
            // their target identifier's *text*, not by node kind.
            Self::Elixir => elixir_target_name(node, bytes).and_then(|t| match t.as_str() {
                "defmodule" => Some(SymbolKind::Module),
                "def" | "defp" | "defmacro" | "defmacrop" => Some(SymbolKind::Function),
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
            // Elixir defs carry no `name` field; the defined name is the head of
            // the first argument — a nested `call` (`def add(a, b)`), a bare
            // `identifier` (`def run`), or an `alias` (`defmodule Math`).
            // An object property bound to a function carries its name in `key`;
            // every other JS/TS function uses `name` (#257).
            (Self::JavaScript | Self::TypeScript | Self::Tsx, SymbolKind::Function) => {
                name_field(node, bytes).or_else(|| {
                    node.child_by_field_name("key")
                        .and_then(|n| node_text(n, bytes))
                        .map(str::to_string)
                })
            }
            (Self::Elixir, _) => elixir_defined_name(node, bytes),
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
            // PHP: plain `f()`, method `$x->m()` / `$x?->m()`, and static `A::b()`.
            Self::Php => matches!(
                node_kind,
                "function_call_expression"
                    | "member_call_expression"
                    | "nullsafe_member_call_expression"
                    | "scoped_call_expression"
            ),
            // Bash "calls" are commands (`helper arg`).
            Self::Bash => node_kind == "command",
            Self::Kotlin | Self::Swift | Self::Scala => node_kind == "call_expression",
            Self::Lua => node_kind == "function_call",
            Self::Elixir => node_kind == "call",
        }
    }

    /// Whether a call reaches its callee through a receiver expression, so the
    /// callee belongs to a value whose type the graph does not know.
    ///
    /// `x.foo()` is [`RefKind::Method`]; `foo()` and path calls like
    /// `a::b::foo()` are [`RefKind::Free`]. The distinction is what stops the
    /// resolver attributing a std or dependency method to a same-named free
    /// function that happens to be the view's only definition (#223).
    ///
    /// Languages whose grammar does not surface a receiver here fall through to
    /// `Free`, which is the behaviour they had before this existed — no worse,
    /// just not yet improved.
    fn callee_kind(self, call: Node<'_>) -> RefKind {
        // A few grammars mark a method call on the call node itself.
        let by_call_node = match self {
            Self::Php => matches!(
                call.kind(),
                "member_call_expression" | "nullsafe_member_call_expression"
            ),
            // `obj.m()` carries an `object`; a bare `m()` does not.
            Self::Java => call.child_by_field_name("object").is_some(),
            // Ruby's `call` names its receiver explicitly.
            Self::Ruby => call.child_by_field_name("receiver").is_some(),
            _ => false,
        };
        if by_call_node {
            return RefKind::Method;
        }

        // Otherwise the shape is visible on the callee expression.
        let callee = match self {
            Self::Kotlin | Self::Swift => call.named_child(0),
            Self::Lua => call.child_by_field_name("name"),
            _ => call.child_by_field_name("function"),
        };
        let Some(callee) = callee else {
            return RefKind::Free;
        };
        // Note what is deliberately absent: Rust `scoped_identifier`, C++
        // `qualified_identifier` and Go's package-qualified `selector_expression`
        // are paths, not receivers. Go cannot tell `pkg.Func()` from `x.Method()`
        // at this level, so it stays Free rather than guessing.
        let member_like = matches!(
            (self, callee.kind()),
            (
                Self::Rust | Self::Scala | Self::C | Self::Cpp,
                "field_expression"
            ) | (Self::Python, "attribute")
                | (
                    Self::JavaScript | Self::TypeScript | Self::Tsx,
                    "member_expression"
                )
                | (Self::CSharp, "member_access_expression")
                | (Self::Kotlin | Self::Swift, "navigation_expression")
                | (
                    Self::Lua,
                    "dot_index_expression" | "method_index_expression"
                )
        );
        if member_like {
            RefKind::Method
        } else {
            RefKind::Free
        }
    }

    /// The path segment immediately before the callee name, for a path-shaped
    /// call — `Language` in `Language::from_extension()`, `b` in `a::b::c()`.
    ///
    /// Only grammars that make a path *unambiguously* a path are handled. Go's
    /// `selector_expression` covers both `pkg.Func()` and `x.Method()` with no
    /// way to tell them apart here, and Java's `object` field likewise covers a
    /// static call and a receiver call — recording `pkg` or `x` as a qualifier
    /// would name a value, not a type. Every other language falls through to
    /// `None`, which is the behaviour it had before this existed (#248).
    ///
    /// A [`RefKind::Method`] call never has one: its receiver is a value whose
    /// type the graph does not know. #251 fills that in where the type is
    /// syntactically visible.
    fn callee_qualifier(self, call: Node<'_>, bytes: &[u8]) -> Option<String> {
        // PHP marks a static call with its own node kind, and puts the scope on
        // the call node rather than on a nested callee.
        if self == Self::Php {
            return (call.kind() == "scoped_call_expression")
                .then(|| call.child_by_field_name("scope"))
                .flatten()
                .and_then(|n| trailing_path_segment(n, bytes));
        }
        let (path_kind, scope_field) = match self {
            Self::Rust => ("scoped_identifier", "path"),
            Self::Cpp => ("qualified_identifier", "scope"),
            _ => return None,
        };
        let callee = call.child_by_field_name("function")?;
        if callee.kind() != path_kind {
            return None;
        }
        callee
            .child_by_field_name(scope_field)
            .and_then(|n| trailing_path_segment(n, bytes))
    }

    /// Names `node` brings into this file's scope, if it is an import node.
    ///
    /// Recorded as a lexical signal rather than a resolved path: an import says
    /// which *module* a name came from, which is enough to prefer one candidate
    /// definition over another without knowing where a crate root is — and so
    /// without extraction reading any file but this one (ADR 0012, #252).
    ///
    /// Rust, Python, Java, Kotlin, JavaScript and TypeScript. Every other
    /// language records nothing and resolves exactly as it did.
    fn imports_at(self, node: Node<'_>, bytes: &[u8]) -> Vec<Import> {
        let line = node.start_position().row + 1;
        let mut out = Vec::new();
        match self {
            Self::Rust if node.kind() == "use_declaration" => {
                if let Some(argument) = node.child_by_field_name("argument") {
                    collect_rust_uses(argument, bytes, &[], &mut out);
                }
            }
            // `from pkg.model import Symbol [as Other]`. A plain
            // `import pkg.model` binds `pkg`, which names no symbol, so it is
            // left alone.
            Self::Python if node.kind() == "import_from_statement" => {
                let module_name = node.child_by_field_name("module_name");
                // `from ..pkg import X` — the dots say how far up from this
                // file's own package to anchor, and are the whole reason a
                // relative import needs no project root (#261).
                let depth = module_name
                    .filter(|m| m.kind() == "relative_import")
                    .and_then(|m| child_of_kind(m, "import_prefix"))
                    .and_then(|prefix| node_text(prefix, bytes))
                    .map_or(0, |dots| dots.chars().filter(|c| *c == '.').count());
                let path = module_name
                    .map(|m| python_module_segments(m, bytes))
                    .unwrap_or_default();
                let mut cursor = node.walk();
                for child in node.children_by_field_name("name", &mut cursor) {
                    let name = match child.kind() {
                        "aliased_import" => child.child_by_field_name("alias"),
                        _ => Some(child),
                    }
                    .and_then(|n| node_text(n, bytes));
                    if let Some(name) = name {
                        out.push(Import {
                            depth,
                            name: name.to_string(),
                            path: path.clone(),
                            line,
                        });
                    }
                }
            }
            // `import a.b.C;` and `import static a.b.C.d;` share one shape: the
            // trailing segment is the name brought into scope — the class in the
            // first case, the member in the second — and the rest is its path.
            Self::Java if node.kind() == "import_declaration" => {
                // `import a.b.*;` introduces names the file never spells out.
                if child_of_kind(node, "asterisk").is_some() {
                    return out;
                }
                let Some(path_node) = child_of_kind(node, "scoped_identifier")
                    .or_else(|| child_of_kind(node, "identifier"))
                else {
                    return out;
                };
                let mut segments = java_path_segments(path_node, bytes);
                if let Some(name) = segments.pop() {
                    out.push(Import {
                        depth: 0,
                        name,
                        path: segments,
                        line,
                    });
                }
            }
            Self::Kotlin if node.kind() == "import" => {
                // The grammar drops the `*`, so `import a.b.*` and `import a.b`
                // parse identically — the source text is the only way to tell
                // them apart.
                if node_text(node, bytes).is_some_and(|t| t.trim_end().ends_with(".*")) {
                    return out;
                }
                let Some(qualified) = child_of_kind(node, "qualified_identifier") else {
                    return out;
                };
                let mut segments: Vec<String> = Vec::new();
                let mut cursor = qualified.walk();
                for child in qualified.named_children(&mut cursor) {
                    if let Some(text) = node_text(child, bytes) {
                        segments.push(text.to_string());
                    }
                }
                // `import a.b.Thing as Other` puts the alias in a bare
                // identifier beside the qualified name; it is the name this
                // file will actually write.
                let mut outer = node.walk();
                let alias = node
                    .named_children(&mut outer)
                    .find(|c| c.kind() == "identifier")
                    .and_then(|n| node_text(n, bytes))
                    .map(str::to_string);
                let name = match alias {
                    Some(alias) => {
                        segments.pop();
                        Some(alias)
                    }
                    None => segments.pop(),
                };
                if let Some(name) = name {
                    out.push(Import {
                        depth: 0,
                        name,
                        path: segments,
                        line,
                    });
                }
            }
            // `#include "a/b.h"` names a *file*, and every declaration in it
            // becomes visible — a stronger signal than any package path,
            // because the file is the thing rather than a convention a path is
            // assumed to mirror. Recorded with no name; resolution matches it
            // by path (#267).
            Self::C | Self::Cpp if node.kind() == "preproc_include" => {
                let path = node
                    .child_by_field_name("path")
                    .and_then(|p| node_text(p, bytes))
                    .map(include_segments)
                    .unwrap_or_default();
                if !path.is_empty() {
                    out.push(Import {
                        depth: 0,
                        name: String::new(),
                        path,
                        line,
                    });
                }
            }
            // CommonJS: `const fs = require('fs')` and its destructuring
            // forms. The module system for a large amount of Node code, and
            // invisible until now (#269).
            Self::JavaScript | Self::TypeScript | Self::Tsx
                if node.kind() == "variable_declarator" =>
            {
                let Some(path) = require_source(node, bytes) else {
                    return out;
                };
                for name in require_bound_names(node, bytes) {
                    out.push(Import {
                        depth: 0,
                        name,
                        path: path.clone(),
                        line,
                    });
                }
            }
            Self::JavaScript | Self::TypeScript | Self::Tsx
                if node.kind() == "import_statement" =>
            {
                let path = node
                    .child_by_field_name("source")
                    .and_then(|s| node_text(s, bytes))
                    .map(js_module_segments)
                    .unwrap_or_default();
                collect_js_import_names(node, bytes, &path, line, &mut out);
            }
            _ => {}
        }
        out
    }

    /// Bare names appearing in a call's argument list, as `(name, line)` —
    /// `helper` in `and_then(helper)`.
    ///
    /// A function used as a value is a path expression, not a call expression,
    /// so it records no edge at all. A function only ever passed as a callback
    /// therefore looked unused, and `unreferenced` reported it as deletable
    /// (#250).
    ///
    /// Only the argument list's own children are inspected, one wrapper deep. A
    /// nested call is its own node and [`walk`] reaches it separately, so
    /// nothing is counted twice, and the callee itself lives in a different
    /// field so it is never picked up here.
    ///
    /// Deliberately over-inclusive: extraction is per-file, so it cannot tell an
    /// identifier naming a function from one naming a local, and records both.
    /// See [`RefKind::Value`] for why that is the safe direction.
    fn value_args(self, call: Node<'_>, bytes: &[u8]) -> Vec<(String, usize)> {
        let Some(args) = call.child_by_field_name("arguments") else {
            return Vec::new();
        };
        let is_ident = |node: &Node<'_>| matches!(node.kind(), "identifier" | "simple_identifier");
        let mut out = Vec::new();
        let mut cursor = args.walk();
        for child in args.named_children(&mut cursor) {
            // Some grammars wrap each argument (C#'s and PHP's `argument`), and
            // `&helper` wraps the name it borrows. Look one level in, never
            // further: a deeper expression is not a bare name.
            let leaf = if is_ident(&child) {
                Some(child)
            } else if child.named_child_count() == 1 {
                child.named_child(0).filter(is_ident)
            } else {
                None
            };
            if let Some(leaf) = leaf
                && let Some(name) = node_text(leaf, bytes)
            {
                out.push((name.to_string(), leaf.start_position().row + 1));
            }
        }
        out
    }

    /// Calls hidden inside an opaque macro-argument node, as `(name, line)`.
    ///
    /// Rust macro arguments parse as a `token_tree` of raw tokens rather than
    /// expressions, so `assert_eq!(f(), 1)` contains no `call_expression` and
    /// the call to `f` is invisible to [`is_call`](Self::is_call). Since
    /// assertions are where much of a codebase is exercised, that silently
    /// removed a large share of the call graph (#216).
    ///
    /// Inside a token tree a call is an `identifier` whose immediate next
    /// sibling is another `token_tree` — `f` followed by `()`. A nested macro
    /// (`matches!(..)`) has a `!` between the two, so it is naturally excluded.
    /// Each token tree inspects only its own direct children, and [`walk`]
    /// recurses into nested trees, so nothing is counted twice.
    ///
    /// This is a token-level heuristic, not type resolution: a token tree is not
    /// type-checked, so a tuple-struct pattern like `Some(_)` reads as a call.
    /// That matches the base graph, which already records constructors and enum
    /// variants as calls.
    fn macro_arg_calls(self, node: Node<'_>, bytes: &[u8]) -> Vec<(String, usize)> {
        if self != Self::Rust || node.kind() != "token_tree" {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() != "identifier" {
                continue;
            }
            let Some(next) = child.next_sibling() else {
                continue;
            };
            // Only a parenthesised tree is an argument list; `vec![..]`'s own
            // brackets belong to the macro, not to a call.
            if next.kind() != "token_tree"
                || !next.utf8_text(bytes).is_ok_and(|t| t.starts_with('('))
            {
                continue;
            }
            if let Some(name) = node_text(child, bytes) {
                out.push((name.to_string(), child.start_position().row + 1));
            }
        }
        out
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
            // Scala `call_expression` holds the callee in a `function` field — a
            // bare `identifier` (`helper(..)`) or a `field_expression`
            // (`obj.method(..)`); take the trailing identifier.
            Self::Scala => last_identifier(func, bytes),
            // PHP `function_call_expression` holds the callee in a `function`
            // field — a `name` (or `qualified_name`) node; take its text.
            Self::Php => node_text(func, bytes).map(str::to_string),
            // Java, Ruby, Bash, Kotlin, Swift, and Lua route through `callee_name`
            // (their callee is a dedicated field/child on the call node, not a
            // nested `function` node); these arms only keep the match exhaustive.
            Self::Java
            | Self::Ruby
            | Self::Bash
            | Self::Kotlin
            | Self::Swift
            | Self::Lua
            | Self::Elixir => node_text(func, bytes).map(str::to_string),
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
            // Kotlin and Swift `call_expression` have no field; the callee is the
            // first named child — an `identifier`/`simple_identifier`
            // (`helper(..)`) or a navigation/member expression (`a.b.method(..)`),
            // whose trailing identifier is the invoked member.
            Self::Kotlin | Self::Swift => call
                .named_child(0)
                .and_then(|callee| last_identifier(callee, bytes)),
            // Lua's `function_call` names the callee in a `name` field — an
            // `identifier` (`helper(..)`) or a dotted/method index (`m.f`/`o:m`),
            // whose trailing identifier is the invoked function.
            Self::Lua => call
                .child_by_field_name("name")
                .and_then(|n| last_identifier(n, bytes)),
            // PHP: plain calls carry the callee in a `function` field (a
            // `name`/`qualified_name`); method (`$x->m()`) and static (`A::b()`)
            // calls carry the invoked member in a `name` field.
            Self::Php => match call.kind() {
                "function_call_expression" => call
                    .child_by_field_name("function")
                    .and_then(|func| self.call_name(func, bytes)),
                _ => call
                    .child_by_field_name("name")
                    .and_then(|n| node_text(n, bytes))
                    .map(str::to_string),
            },
            // Elixir: every `call` carries its callee in a `target` field. A
            // definition call (`def`/`defp`/`defmacro`/`defmacrop`/`defmodule`)
            // and a definition *head* (`add(a, b)` in `def add(a, b)`) are not
            // references; every other call is, keyed by the target's trailing
            // identifier (`helper` for `helper(..)`, `add` for `Mod.add(..)`).
            Self::Elixir => match elixir_target_name(call, bytes) {
                Some(name)
                    if !matches!(
                        name.as_str(),
                        "def" | "defp" | "defmacro" | "defmacrop" | "defmodule"
                    ) && !elixir_is_def_head(call, bytes) =>
                {
                    Some(name)
                }
                _ => None,
            },
            _ => call
                .child_by_field_name("function")
                .and_then(|func| self.call_name(func, bytes)),
        }
    }
}

/// The last `identifier`/`simple_identifier` in `node`'s subtree (depth-first).
/// For a Kotlin/Swift callee that is a bare identifier this is the node itself;
/// for a navigation/member expression (`a.b.method`) it is the trailing member.
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

/// The trailing segment of a path node: `b` for `a::b`, `Language` for a bare
/// `Language`. A nested path names its own last segment in a `name` field; a
/// leaf is its own text (#248).
fn trailing_path_segment(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|n| node_text(n, bytes))
        .or_else(|| node_text(node, bytes))
        .map(str::to_string)
}

/// The name a call site would use to qualify a member of this item: the base
/// type, without generic arguments and without a leading path. `impl<T>
/// Holder<T>` owns `Holder`, and `impl crate::model::Widget` owns `Widget`.
///
/// A call is written `Holder::get()`, never `Holder<T>::get()`, and its
/// qualifier is a single segment, so the stored owner has to be reduced to the
/// same shape or it could never match (#248).
fn owner_key(name: &str) -> String {
    let base = name.split('<').next().unwrap_or(name).trim();
    base.rsplit("::").next().unwrap_or(base).trim().to_string()
}

/// Whether a symbol of this kind owns the symbols defined inside it, so a
/// method records the type it hangs off and a function in an inline module
/// records the module (#248).
fn establishes_ownership(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Impl
            | SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::Trait
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Module
    )
}

/// The first named child of `node` whose kind is `kind`, if any.
fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|c| c.kind() == kind)
}

/// The trailing identifier text of an Elixir `call` node's `target` — a bare
/// `identifier` (`helper(..)`) or the `right` member of a `dot` (`Mod.fun(..)`).
/// `None` when `node` is not a call (no `target` field).
fn elixir_target_name(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let target = node.child_by_field_name("target")?;
    last_identifier(target, bytes)
}

/// The name defined by an Elixir definition call. The signature is the first
/// argument: a nested `call` (`def add(a, b)` → `add`), a bare `identifier`
/// (`def run` → `run`), or an `alias` (`defmodule Math` → `Math`).
fn elixir_defined_name(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let head = child_of_kind(node, "arguments")?.named_child(0)?;
    match head.kind() {
        "call" => elixir_target_name(head, bytes),
        _ => last_identifier(head, bytes).or_else(|| node_text(head, bytes).map(str::to_string)),
    }
}

/// Whether an Elixir `call` is the *head* of a definition — the first argument
/// of a `def`/`defp`/`defmacro`/`defmacrop`/`defmodule` call (e.g. `add(a, b)`
/// in `def add(a, b)`). Such a head names the defined symbol, not a call.
fn elixir_is_def_head(node: Node<'_>, bytes: &[u8]) -> bool {
    let Some(args) = node.parent().filter(|p| p.kind() == "arguments") else {
        return false;
    };
    if args.named_child(0).map(|h| h.id()) != Some(node.id()) {
        return false;
    }
    let Some(def_call) = args.parent().filter(|g| g.kind() == "call") else {
        return false;
    };
    matches!(
        elixir_target_name(def_call, bytes).as_deref(),
        Some("def" | "defp" | "defmacro" | "defmacrop" | "defmodule")
    )
}

/// JavaScript declaration node kinds shared by JS and TS/TSX.
fn js_item_kind(node: Node<'_>, bytes: &[u8]) -> Option<SymbolKind> {
    match node.kind() {
        "function_declaration" | "generator_function_declaration" | "method_definition" => {
            Some(SymbolKind::Function)
        }
        "class_declaration" | "abstract_class_declaration" => Some(SymbolKind::Class),
        // `const foo = () => {}`, class-field `foo = () => {}`, object property
        // `{ foo: () => {} }`, and a function wrapped in a higher-order call —
        // `const C = React.memo(() => {})`. A binding whose value is, or wraps,
        // a function literal is a named function (#257).
        "variable_declarator" | "public_field_definition" | "pair" => node
            .child_by_field_name("value")
            .filter(|value| js_binds_a_function(*value, bytes, 3))
            .map(|_| SymbolKind::Function),
        _ => None,
    }
}

/// Whether a JS/TS initializer is, or wraps, a function literal.
///
/// Direct for `() => {}` and `function () {}`. Through a higher-order call for
/// `memo(() => {})` and `memo(forwardRef(() => {}))`, the idiom that dominates
/// React code and that used to leave whole component files with no symbols at
/// all (#257).
///
/// The wrapper must be called on a **bare identifier** (`memo`, `forwardRef`,
/// `styled`) or on a **capitalized namespace** (`React.memo`). That is what
/// keeps `items.map(x => f(x))` out: an iterator method returns an array, not a
/// function, and treating `const total = items.map(..)` as a function would
/// attribute everything inside the lambda to `total` instead of to the function
/// actually containing it — trading a missing answer for a wrong one.
fn js_binds_a_function(node: Node<'_>, bytes: &[u8], depth: usize) -> bool {
    match node.kind() {
        "arrow_function" | "function_expression" | "generator_function" => true,
        "call_expression" if depth > 0 && js_is_wrapper_call(node, bytes) => {
            node.child_by_field_name("arguments").is_some_and(|args| {
                let mut cursor = args.walk();
                args.named_children(&mut cursor)
                    .any(|arg| js_binds_a_function(arg, bytes, depth - 1))
            })
        }
        _ => false,
    }
}

/// Whether a JS/TS call looks like a wrapper rather than a method on a value.
fn js_is_wrapper_call(call: Node<'_>, bytes: &[u8]) -> bool {
    let Some(callee) = call.child_by_field_name("function") else {
        return false;
    };
    match callee.kind() {
        "identifier" => true,
        // `React.memo` — a capitalized object is a namespace by convention,
        // where `items.map` is a value.
        "member_expression" => callee
            .child_by_field_name("object")
            .filter(|object| object.kind() == "identifier")
            .and_then(|object| node_text(object, bytes))
            .is_some_and(|name| name.starts_with(char::is_uppercase)),
        _ => false,
    }
}

/// Swift `class_declaration` keyword (`struct`/`enum`/`actor`/`class`) → kind.
/// `actor` (a reference type) and `class` both read as Class.
fn swift_type_kind(node: Node<'_>) -> SymbolKind {
    match node
        .child_by_field_name("declaration_kind")
        .map(|k| k.kind())
    {
        Some("struct") => SymbolKind::Struct,
        Some("enum") => SymbolKind::Enum,
        _ => SymbolKind::Class,
    }
}

/// Kotlin `class_declaration` is an `interface` when it has a leading
/// `interface` keyword child; otherwise a `class`.
fn kotlin_class_kind(node: Node<'_>) -> SymbolKind {
    let mut cursor = node.walk();
    if node.children(&mut cursor).any(|c| c.kind() == "interface") {
        SymbolKind::Interface
    } else {
        SymbolKind::Class
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
    walk(
        language,
        tree.root_node(),
        src.as_bytes(),
        None,
        None,
        &BTreeMap::new(),
        &mut graph,
    );
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

/// Local names a JS/TS import clause binds, under module path `path`.
fn collect_js_import_names(
    node: Node<'_>,
    bytes: &[u8],
    path: &[String],
    line: usize,
    out: &mut Vec<Import>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            // `import { A, B as C } from '...'` — the specifier's alias wins.
            "import_specifier" => {
                let name = child
                    .child_by_field_name("alias")
                    .or_else(|| child.child_by_field_name("name"))
                    .and_then(|n| node_text(n, bytes));
                if let Some(name) = name {
                    out.push(Import {
                        depth: 0,
                        name: name.to_string(),
                        path: path.to_vec(),
                        line,
                    });
                }
            }
            // `import Default from '...'`.
            "identifier" => {
                if let Some(name) = node_text(child, bytes) {
                    out.push(Import {
                        depth: 0,
                        name: name.to_string(),
                        path: path.to_vec(),
                        line,
                    });
                }
            }
            // `import_clause`, `named_imports` — containers; look inside.
            "import_clause" | "named_imports" => {
                collect_js_import_names(child, bytes, path, line, out);
            }
            _ => {}
        }
    }
}

/// The module segments a Rust path node names, with relative markers dropped:
/// `crate::model` is `["model"]`, `a::b` is `["a", "b"]`.
fn rust_path_segments(node: Node<'_>, bytes: &[u8]) -> Vec<String> {
    match node.kind() {
        "scoped_identifier" => {
            let mut segments = node
                .child_by_field_name("path")
                .map(|p| rust_path_segments(p, bytes))
                .unwrap_or_default();
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| node_text(n, bytes))
            {
                segments.push(name.to_string());
            }
            segments
        }
        // Relative markers position the path; they name no module (#252).
        "crate" | "self" | "super" => Vec::new(),
        _ => node_text(node, bytes)
            .filter(|t| !t.contains(':'))
            .map(|t| vec![t.to_string()])
            .unwrap_or_default(),
    }
}

/// Names a Rust `use` tree brings into scope, under `prefix`.
///
/// A glob (`use a::*`) contributes nothing: it introduces names this file never
/// spells out, so there is no name to key on.
fn collect_rust_uses(node: Node<'_>, bytes: &[u8], prefix: &[String], out: &mut Vec<Import>) {
    let line = node.start_position().row + 1;
    match node.kind() {
        "scoped_identifier" => {
            let mut segments = rust_path_segments(node, bytes);
            if let Some(name) = segments.pop() {
                let mut path = prefix.to_vec();
                path.append(&mut segments);
                out.push(Import {
                    depth: 0,
                    name,
                    path,
                    line,
                });
            }
        }
        "use_as_clause" => {
            let Some(alias) = node
                .child_by_field_name("alias")
                .and_then(|n| node_text(n, bytes))
            else {
                return;
            };
            let mut segments = node
                .child_by_field_name("path")
                .map(|p| rust_path_segments(p, bytes))
                .unwrap_or_default();
            segments.pop(); // the original name; the alias replaces it
            let mut path = prefix.to_vec();
            path.append(&mut segments);
            out.push(Import {
                depth: 0,
                name: alias.to_string(),
                path,
                line,
            });
        }
        "scoped_use_list" => {
            let mut path = prefix.to_vec();
            if let Some(inner) = node.child_by_field_name("path") {
                path.extend(rust_path_segments(inner, bytes));
            }
            if let Some(list) = node.child_by_field_name("list") {
                collect_rust_uses(list, bytes, &path, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_rust_uses(child, bytes, prefix, out);
            }
        }
        "use_wildcard" => {}
        // A bare `use foo;`.
        "identifier" => {
            if let Some(name) = node_text(node, bytes) {
                out.push(Import {
                    depth: 0,
                    name: name.to_string(),
                    path: prefix.to_vec(),
                    line,
                });
            }
        }
        _ => {}
    }
}

/// The dotted segments a Java `scoped_identifier` names, outermost first.
/// Same shape as Rust's, under different field names (`scope`/`name`).
fn java_path_segments(node: Node<'_>, bytes: &[u8]) -> Vec<String> {
    match node.kind() {
        "scoped_identifier" => {
            let mut segments = node
                .child_by_field_name("scope")
                .map(|s| java_path_segments(s, bytes))
                .unwrap_or_default();
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| node_text(n, bytes))
            {
                segments.push(name.to_string());
            }
            segments
        }
        _ => node_text(node, bytes)
            .filter(|t| !t.contains('.'))
            .map(|t| vec![t.to_string()])
            .unwrap_or_default(),
    }
}

/// The module segments a Python dotted name or relative import names.
fn python_module_segments(node: Node<'_>, bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                if let Some(text) = node_text(child, bytes) {
                    out.push(text.to_string());
                }
            }
            // `from .model import X` nests the dotted name under the relative
            // import; the leading dots are a marker, not a module.
            "dotted_name" => out.extend(python_module_segments(child, bytes)),
            _ => {}
        }
    }
    out
}

/// The module segments a `require(...)` initializer names, or `None` when the
/// declarator is not a require or its argument is not a literal path.
///
/// A computed `require(name)` names no module, so it records nothing — the same
/// reasoning as a glob import, which introduces names the file never spells out.
fn require_source(declarator: Node<'_>, bytes: &[u8]) -> Option<Vec<String>> {
    let value = declarator.child_by_field_name("value")?;
    if value.kind() != "call_expression" {
        return None;
    }
    let callee = value.child_by_field_name("function")?;
    if callee.kind() != "identifier" || node_text(callee, bytes) != Some("require") {
        return None;
    }
    let argument = value.child_by_field_name("arguments")?.named_child(0)?;
    if argument.kind() != "string" {
        return None;
    }
    let segments = js_module_segments(node_text(argument, bytes)?);
    (!segments.is_empty()).then_some(segments)
}

/// The local names a `require` declarator binds: one for `const fs = ...`, one
/// per property for `const { a, b } = ...`, and the *local* half of a rename.
fn require_bound_names(declarator: Node<'_>, bytes: &[u8]) -> Vec<String> {
    let Some(pattern) = declarator.child_by_field_name("name") else {
        return Vec::new();
    };
    match pattern.kind() {
        "identifier" => node_text(pattern, bytes)
            .map(|name| vec![name.to_string()])
            .unwrap_or_default(),
        "object_pattern" => {
            let mut out = Vec::new();
            let mut cursor = pattern.walk();
            for child in pattern.named_children(&mut cursor) {
                let named = match child.kind() {
                    "shorthand_property_identifier_pattern" => Some(child),
                    // `{ a: c }` — `c` is what this file writes.
                    "pair_pattern" => child.child_by_field_name("value"),
                    _ => None,
                };
                if let Some(name) = named.and_then(|n| node_text(n, bytes)) {
                    out.push(name.to_string());
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// The path segments a C/C++ include names: `"engine/render/pipeline.h"` is
/// `["engine", "render", "pipeline"]`, `<vector>` is `["vector"]`.
///
/// The extension is dropped so an include of a header matches the translation
/// unit that defines what it declares — a prototype in a `.h` is not a symbol,
/// the definition in the `.c` is, and both live at the same path stem (#267).
fn include_segments(raw: &str) -> Vec<String> {
    raw.trim_matches(|c| c == '"' || c == '<' || c == '>')
        .split('/')
        .filter(|part| !matches!(*part, "" | "." | ".."))
        .map(|part| part.rsplit_once('.').map_or(part, |(stem, _)| stem))
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// The module segments a JS/TS import source string names: `'./model'` is
/// `["model"]`, `'pkg/sub/model.js'` is `["pkg", "sub", "model"]`.
///
/// A bare `@` is a tsconfig path alias rooted at the project (`@/lib/utils`),
/// so like `.` and `..` it positions the path rather than naming a directory.
/// A *scoped package* leads with `@scope`, which is a real name and survives.
fn js_module_segments(raw: &str) -> Vec<String> {
    raw.trim_matches(|c| c == '\'' || c == '"' || c == '`')
        .split('/')
        .filter(|part| !matches!(*part, "" | "." | ".." | "@" | "~"))
        .map(|part| part.rsplit_once('.').map_or(part, |(stem, _)| stem))
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// The base type a Rust type node names, or `None` when it needs inference:
/// `Widget` for `Widget`, `&Widget`, `&mut Widget` and `a::b::Widget`, `Vec` for
/// `Vec<T>`. `impl Trait`, `dyn Trait` and tuples yield nothing (#251).
fn rust_type_name(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" => node_text(node, bytes).map(str::to_string),
        "reference_type" => node
            .child_by_field_name("type")
            .and_then(|t| rust_type_name(t, bytes)),
        // `generic_type` names its base in `type`; `scoped_type_identifier`
        // names its last segment in `name`.
        "generic_type" | "scoped_type_identifier" => node
            .child_by_field_name("type")
            .or_else(|| node.child_by_field_name("name"))
            .and_then(|t| rust_type_name(t, bytes)),
        _ => None,
    }
}

/// The type a Rust initializer expression obviously produces, syntactically:
/// `Widget::new(..)` and `Widget { .. }` both name `Widget`.
///
/// A plain call (`make()`) names nothing without a return type, and a chain
/// (`Widget::new().wrap()`) is deliberately not followed — the last call decides
/// the type and the graph cannot know it (#251).
fn rust_value_type(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "call_expression" => {
            let func = node.child_by_field_name("function")?;
            (func.kind() == "scoped_identifier")
                .then(|| func.child_by_field_name("path"))
                .flatten()
                .and_then(|path| trailing_path_segment(path, bytes))
        }
        "struct_expression" => node
            .child_by_field_name("name")
            .and_then(|n| rust_type_name(n, bytes)),
        "reference_expression" => node
            .child_by_field_name("value")
            .and_then(|v| rust_value_type(v, bytes)),
        _ => None,
    }
}

/// Type parameters declared on a Rust function (`fn g<T: Tr>`).
///
/// A receiver typed `T` names no type in the view, and recording it would
/// invite the resolver to pick an impl — the guess #223 removed. Excluded so
/// those receivers keep declining.
fn rust_type_parameters(func: Node<'_>, bytes: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(params) = func.child_by_field_name("type_parameters") else {
        return out;
    };
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        // `T`, `T: Bound`, `T = Default`, `T: A + B` all lead with the
        // parameter's own name, so the first `type_identifier` in the subtree is
        // it. Lifetimes and const parameters have none and drop out.
        let name = first_type_identifier(child, bytes);
        if let Some(name) = name {
            out.insert(name);
        }
    }
    out
}

/// The first `type_identifier` in `node`'s subtree, depth-first.
fn first_type_identifier(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    if node.kind() == "type_identifier" {
        return node_text(node, bytes).map(str::to_string);
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find_map(|child| first_type_identifier(child, bytes))
}

/// `let` bindings in a Rust body whose type is written or obvious, added to
/// `out`. Does not descend into a nested `function_item`: that body is its own
/// scope and gets its own table.
fn collect_rust_lets(
    node: Node<'_>,
    bytes: &[u8],
    generics: &BTreeSet<String>,
    out: &mut BTreeMap<String, String>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "function_item" {
            continue;
        }
        if child.kind() == "let_declaration"
            && let Some(pattern) = child.child_by_field_name("pattern")
            && pattern.kind() == "identifier"
            && let Some(name) = node_text(pattern, bytes)
        {
            // An explicit annotation beats the initializer.
            let ty = child
                .child_by_field_name("type")
                .and_then(|t| rust_type_name(t, bytes))
                .or_else(|| {
                    child
                        .child_by_field_name("value")
                        .and_then(|v| rust_value_type(v, bytes))
                });
            if let Some(ty) = ty
                && !generics.contains(&ty)
            {
                out.insert(name.to_string(), ty);
            }
        }
        collect_rust_lets(child, bytes, generics, out);
    }
}

/// Receiver types visible inside one Rust function body, by binding name.
///
/// Three shapes carry most of the volume and are all purely syntactic: a
/// parameter with a written type, a binding initialized from a constructor-style
/// call, and a binding initialized from a struct literal. Anything needing
/// inference — a return type, a generic, a trait object — is left out, so those
/// receivers keep declining and keep being counted (#251).
///
/// Rust only. Every other language records no receiver type and behaves exactly
/// as it did.
fn rust_receiver_types(func: Node<'_>, bytes: &[u8]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let generics = rust_type_parameters(func, bytes);

    if let Some(params) = func.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for param in params.named_children(&mut cursor) {
            let (Some(pattern), Some(ty)) = (
                param.child_by_field_name("pattern"),
                param.child_by_field_name("type"),
            ) else {
                continue;
            };
            if let Some(name) = node_text(pattern, bytes)
                && let Some(ty) = rust_type_name(ty, bytes)
                && !generics.contains(&ty)
            {
                out.insert(name.to_string(), ty);
            }
        }
    }

    if let Some(body) = func.child_by_field_name("body") {
        collect_rust_lets(body, bytes, &generics, &mut out);
    }
    out
}

/// The bare receiver of a Rust method call — `w` in `w.get()` — or `None` when
/// the receiver is an expression rather than a name.
fn rust_receiver_name(call: Node<'_>, bytes: &[u8]) -> Option<String> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "field_expression" {
        return None;
    }
    let value = func.child_by_field_name("value")?;
    matches!(value.kind(), "identifier" | "self")
        .then(|| node_text(value, bytes))
        .flatten()
        .map(str::to_string)
}

fn walk(
    language: Language,
    node: Node<'_>,
    bytes: &[u8],
    current_fn: Option<&str>,
    current_type: Option<&str>,
    current_receivers: &BTreeMap<String, String>,
    graph: &mut CodeGraph,
) {
    let mut enclosing = current_fn.map(str::to_string);
    let mut owner = current_type.map(str::to_string);
    // Assigned only when this node opens a function body, and only then does
    // `receivers` point at it — so the table is scoped to the body it came from
    // and cannot leak into a sibling function (#251).
    let scoped_receivers;
    let mut receivers = current_receivers;

    if let Some(kind) = language.item_kind(node, bytes)
        && let Some(name) = language.item_name(node, kind, bytes)
    {
        graph.symbols.push(Symbol {
            name: name.clone(),
            kind,
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            // The owner in scope *around* this item — an item is not its own
            // owner, exactly as a function is not its own enclosing function.
            owner: owner.clone(),
        });
        if kind == SymbolKind::Function {
            enclosing = Some(name.clone());
            if language == Language::Rust {
                scoped_receivers = rust_receiver_types(node, bytes);
                receivers = &scoped_receivers;
            }
        }
        if establishes_ownership(kind) {
            owner = Some(owner_key(&name));
        }
    }

    if language.is_call(node.kind())
        && let Some(name) = language.callee_name(node, bytes)
    {
        let kind = language.callee_kind(node);
        graph.references.push(Reference {
            name,
            from: enclosing.clone(),
            line: node.start_position().row + 1,
            kind,
            // A receiver is a value, not a type (#248); a value reference is
            // not a call site at all (#250).
            qualifier: match kind {
                RefKind::Value => None,
                // `x.foo()` belongs to whatever `x` is. Where the body says so
                // outright, say so; otherwise keep declining (#251).
                RefKind::Method if language == Language::Rust => rust_receiver_name(node, bytes)
                    .and_then(|recv| match recv.as_str() {
                        "self" => owner.clone(),
                        other => receivers.get(other).cloned(),
                    }),
                RefKind::Method => None,
                RefKind::Free => language.callee_qualifier(node, bytes),
            },
        });
    }

    // Names this file brings into scope (#252).
    for import in language.imports_at(node, bytes) {
        graph.imports.push(import);
    }

    // Names handed to a call as values rather than called — `and_then(helper)`
    // records no call edge, so without this a live callback looks dead (#250).
    if language.is_call(node.kind()) {
        for (name, line) in language.value_args(node, bytes) {
            graph.references.push(Reference {
                name,
                from: enclosing.clone(),
                line,
                kind: RefKind::Value,
                qualifier: None,
            });
        }
    }

    // Calls the grammar hides inside an opaque macro-argument node (#216).
    for (name, line) in language.macro_arg_calls(node, bytes) {
        graph.references.push(Reference {
            name,
            from: enclosing.clone(),
            line,
            // A token-tree call is a bare `ident(` by construction (#216).
            kind: RefKind::Free,
            qualifier: None,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(
            language,
            child,
            bytes,
            enclosing.as_deref(),
            owner.as_deref(),
            receivers,
            graph,
        );
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

    // ---- calls inside macro arguments (#216) ------------------------------

    /// Names referenced in `src`, for the macro-argument cases below.
    fn rust_ref_names(src: &str) -> Vec<String> {
        build_rust(src)
            .references
            .iter()
            .map(|r| r.name.clone())
            .collect()
    }

    #[test]
    fn rust_records_a_call_inside_assert_eq() {
        // Rust macro bodies parse as token trees, so this call used to vanish —
        // the dominant false positive behind `unreferenced` (#216).
        let names = rust_ref_names("fn g() { assert_eq!(f(), 1); }");
        assert!(names.contains(&"f".to_string()), "got {names:?}");
    }

    #[test]
    fn rust_records_calls_inside_common_macros() {
        for (src, want) in [
            ("fn g() { println!(\"{}\", h()); }", "h"),
            ("fn g() { let v = vec![mk()]; }", "mk"),
            ("fn g() { panic!(\"{}\", why()); }", "why"),
            ("fn g() { write!(w, \"{}\", val()); }", "val"),
        ] {
            let names = rust_ref_names(src);
            assert!(names.contains(&want.to_string()), "{want} in {names:?}");
        }
    }

    #[test]
    fn rust_records_a_qualified_call_inside_a_macro_by_last_segment() {
        // The real #216 repro: `Language::from_extension` is called only from
        // assertions, so it looked uncalled.
        let names =
            rust_ref_names("fn g() { assert_eq!(Language::from_extension(\"rs\"), None); }");
        assert!(names.contains(&"from_extension".to_string()), "{names:?}");
    }

    #[test]
    fn rust_records_nested_calls_inside_a_macro() {
        let names = rust_ref_names("fn g() { assert_eq!(outer(inner()), 1); }");
        assert!(names.contains(&"outer".to_string()), "{names:?}");
        assert!(names.contains(&"inner".to_string()), "{names:?}");
    }

    #[test]
    fn rust_records_a_macro_arg_call_once() {
        let names = rust_ref_names("fn g() { assert_eq!(f(), 1); }");
        assert_eq!(
            names.iter().filter(|n| *n == "f").count(),
            1,
            "nested token trees must not double-count: {names:?}"
        );
    }

    #[test]
    fn rust_macro_arg_calls_carry_the_enclosing_function() {
        let g = build_rust("fn outer_fn() { assert_eq!(f(), 1); }");
        let r = g.references.iter().find(|r| r.name == "f").expect("f");
        assert_eq!(r.from.as_deref(), Some("outer_fn"));
    }

    #[test]
    fn rust_does_not_treat_a_nested_macro_name_as_a_call() {
        // `matches!` is a macro, not a function: the `!` between the identifier
        // and the token tree is what distinguishes them.
        let names = rust_ref_names("fn g() { assert!(matches!(a, Some(_))); }");
        assert!(!names.contains(&"matches".to_string()), "{names:?}");
    }

    #[test]
    fn rust_does_not_invent_calls_from_plain_macro_arguments() {
        // Bare identifiers and literals are not calls — only an identifier
        // immediately followed by a parenthesised token tree is.
        let names = rust_ref_names("fn g() { println!(\"{}\", x); }");
        assert!(!names.contains(&"x".to_string()), "{names:?}");
        let names = rust_ref_names("fn g() { let v = vec![1, 2, 3]; }");
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn rust_still_records_ordinary_calls_alongside_macro_ones() {
        let names = rust_ref_names("fn g() { plain(); assert_eq!(inside(), 1); }");
        assert!(names.contains(&"plain".to_string()), "{names:?}");
        assert!(names.contains(&"inside".to_string()), "{names:?}");
    }

    #[test]
    fn rust_macro_arg_call_line_is_one_based() {
        let g = build_rust("fn g() {\n    assert_eq!(f(), 1);\n}");
        let r = g.references.iter().find(|r| r.name == "f").expect("f");
        assert_eq!(r.line, 2);
    }

    // ---- grammar audit for the same opaque-node hole (#216) ----------------

    #[test]
    fn c_records_macro_invocations_at_the_call_site() {
        // A C macro *use* is indistinguishable from a call to the grammar, so
        // it and its neighbours are recorded — no Rust-style hole here.
        let g = build(
            Language::C,
            "#define M() foo()\nvoid g(void) { M(); bar(); }",
        );
        let names: Vec<&str> = g.references.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"M"));
        assert!(names.contains(&"bar"));
    }

    #[test]
    fn c_does_not_record_calls_inside_a_define_body() {
        // The one analogous hole the audit found: a `#define` body is a single
        // opaque `preproc_arg` token, not an expression tree, so `foo()` here is
        // invisible. Unlike Rust's token tree there are no child nodes to read,
        // so recovering it means lexing macro text — deliberately out of scope.
        // Pinned so the gap is discoverable rather than silent.
        let g = build(Language::C, "#define M() foo()\nvoid g(void) { M(); }");
        let names: Vec<&str> = g.references.iter().map(|r| r.name.as_str()).collect();
        assert!(!names.contains(&"foo"), "known gap: {names:?}");
    }

    #[test]
    fn elixir_records_calls_inside_a_quote_block() {
        // `quote do: foo()` parses as real expressions — no hole.
        let g = build(
            Language::Elixir,
            "defmodule A do\n  def g do\n    quote do: foo()\n  end\nend",
        );
        let names: Vec<&str> = g.references.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"foo"), "{names:?}");
    }

    #[test]
    fn ruby_records_calls_inside_a_block() {
        let g = build(Language::Ruby, "def g\n  define_method(:x) { foo() }\nend");
        let names: Vec<&str> = g.references.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"foo"), "{names:?}");
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
        // A header goes through the wider grammar; see #266.
        assert_eq!(Language::from_extension("h"), Some(Language::Cpp));
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
        assert_eq!(Language::from_extension("swift"), Some(Language::Swift));
        assert_eq!(Language::from_extension("lua"), Some(Language::Lua));
        assert_eq!(Language::from_extension("scala"), Some(Language::Scala));
        assert_eq!(Language::from_extension("ex"), Some(Language::Elixir));
        assert_eq!(Language::from_extension("exs"), Some(Language::Elixir));
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

    const SWIFT_SRC: &str = r#"
class Widget {
    func area() -> Int {
        return helper(1)
    }
}

protocol Shape {}

func helper(x: Int) -> Int {
    return x + 1
}

func main() {
    helper(2)
}
"#;

    #[test]
    fn swift_extracts_definitions() {
        let g = build(Language::Swift, SWIFT_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("Widget"), Some(SymbolKind::Class));
        assert_eq!(named("Shape"), Some(SymbolKind::Interface)); // `protocol`
        assert_eq!(named("area"), Some(SymbolKind::Function));
        assert_eq!(named("helper"), Some(SymbolKind::Function));
        assert_eq!(named("main"), Some(SymbolKind::Function));
    }

    #[test]
    fn swift_records_call_with_enclosing_fn() {
        let g = build(Language::Swift, SWIFT_SRC);
        // `helper(2)` -> call_expression whose first child is a `simple_identifier`,
        // from `main`.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("main")),
            "helper() call from main"
        );
    }

    const LUA_SRC: &str = r#"
function helper(x)
  return x + 1
end

function main()
  return helper(2)
end
"#;

    #[test]
    fn lua_extracts_definitions() {
        let g = build(Language::Lua, LUA_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("helper"), Some(SymbolKind::Function));
        assert_eq!(named("main"), Some(SymbolKind::Function));
    }

    #[test]
    fn lua_records_call_with_enclosing_fn() {
        let g = build(Language::Lua, LUA_SRC);
        // `helper(2)` -> function_call whose `name` field is the callee, from `main`.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("main")),
            "helper() call from main"
        );
    }

    const SCALA_SRC: &str = r#"
class Widget {
  def area(): Int = { helper(1) }
}

object Config

trait Shape

def helper(x: Int): Int = x + 1

def main(): Unit = { helper(2) }
"#;

    #[test]
    fn scala_extracts_definitions() {
        let g = build(Language::Scala, SCALA_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("Widget"), Some(SymbolKind::Class));
        assert_eq!(named("Config"), Some(SymbolKind::Class)); // `object` singleton
        assert_eq!(named("Shape"), Some(SymbolKind::Trait));
        assert_eq!(named("area"), Some(SymbolKind::Function));
        assert_eq!(named("helper"), Some(SymbolKind::Function));
        assert_eq!(named("main"), Some(SymbolKind::Function));
    }

    #[test]
    fn scala_records_call_with_enclosing_fn() {
        let g = build(Language::Scala, SCALA_SRC);
        // `helper(2)` -> call_expression `function` field, from `main`.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("main")),
            "helper() call from main"
        );
    }

    const ELIXIR_SRC: &str = r#"
defmodule Math do
  def add(a, b) do
    helper(a) + b
  end

  defp helper(x), do: x

  def run do
    Remote.compute(1)
  end
end
"#;

    #[test]
    fn elixir_extracts_definitions() {
        let g = build(Language::Elixir, ELIXIR_SRC);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        // `defmodule Math` -> Module (name is the `alias`).
        assert_eq!(named("Math"), Some(SymbolKind::Module));
        // `def add(a, b)` -> Function (name is the nested-call head).
        assert_eq!(named("add"), Some(SymbolKind::Function));
        // `defp helper(x)` -> Function.
        assert_eq!(named("helper"), Some(SymbolKind::Function));
        // `def run` (no parens) -> Function (name is a bare identifier head).
        assert_eq!(named("run"), Some(SymbolKind::Function));
    }

    #[test]
    fn elixir_records_calls() {
        let g = build(Language::Elixir, ELIXIR_SRC);
        // `helper(a)` -> reference to `helper` from inside `add`; the def head
        // `add(a, b)` is not itself recorded as a call.
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "helper" && r.from.as_deref() == Some("add")),
            "helper() call from add"
        );
        // Remote call `Remote.compute(1)` -> reference to `compute` from `run`
        // (the target's trailing identifier).
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "compute" && r.from.as_deref() == Some("run")),
            "Remote.compute() call from run"
        );
        // The definition heads must not leak in as self-calls.
        assert!(
            !g.references.iter().any(|r| r.name == "add"),
            "def head add(a, b) not recorded as a call"
        );
    }

    // #136: JS/TS arrow-function and function-expression bindings are named
    // functions; a `variable_declarator`/`public_field_definition` whose value
    // is an `arrow_function`/`function_expression`.
    #[test]
    fn javascript_extracts_arrow_and_function_expression_bindings() {
        let src = r#"
const foo = () => { bar(); };
const baz = function () { qux(); };
"#;
        let g = build(Language::JavaScript, src);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("foo"), Some(SymbolKind::Function));
        assert_eq!(named("baz"), Some(SymbolKind::Function));
        // Calls inside are attributed to the binding name (`walk` sets enclosing).
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "bar" && r.from.as_deref() == Some("foo")),
            "bar() call attributed to foo"
        );
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "qux" && r.from.as_deref() == Some("baz")),
            "qux() call attributed to baz"
        );
    }

    #[test]
    fn typescript_extracts_arrow_bindings_and_class_fields() {
        // A `const` arrow binding and a class-field arrow (`public_field_definition`).
        let src = r#"
const foo = (): void => { bar(); };
class C { handler = (): void => { onClick(); }; }
"#;
        let g = build(Language::TypeScript, src);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("foo"), Some(SymbolKind::Function));
        assert_eq!(named("handler"), Some(SymbolKind::Function));
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "bar" && r.from.as_deref() == Some("foo")),
            "bar() call attributed to foo"
        );
        assert!(
            g.references
                .iter()
                .any(|r| r.name == "onClick" && r.from.as_deref() == Some("handler")),
            "onClick() call attributed to handler"
        );
    }

    // #137: PHP method (`$this->m()`, `member_call_expression`) and static
    // (`A::b()`, `scoped_call_expression`) calls are recorded.
    #[test]
    fn php_records_method_and_static_calls() {
        let src = r#"<?php
class A {
    function run() {
        $this->other();
        self::x();
        B::stat();
    }
}
"#;
        let g = build(Language::Php, src);
        let called = |n: &str| {
            g.references
                .iter()
                .any(|r| r.name == n && r.from.as_deref() == Some("run"))
        };
        assert!(called("other"), "$this->other() recorded from run");
        assert!(called("x"), "self::x() recorded from run");
        assert!(called("stat"), "B::stat() recorded from run");
    }

    // #151: Swift struct/enum/actor and Kotlin interface/object are no longer all
    // mislabeled Class.
    #[test]
    fn swift_distinguishes_struct_enum_class() {
        let src = r#"
struct Point { var x: Int }
enum Color { case red }
class Widget {}
actor Worker {}
protocol Shape {}
"#;
        let g = build(Language::Swift, src);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("Point"), Some(SymbolKind::Struct));
        assert_eq!(named("Color"), Some(SymbolKind::Enum));
        assert_eq!(named("Widget"), Some(SymbolKind::Class));
        assert_eq!(named("Worker"), Some(SymbolKind::Class)); // `actor` -> Class
        assert_eq!(named("Shape"), Some(SymbolKind::Interface)); // `protocol`
    }

    #[test]
    fn kotlin_distinguishes_interface_from_class() {
        let src = r#"
interface Shape { }
class Widget { }
object Config
"#;
        let g = build(Language::Kotlin, src);
        let named = |n: &str| g.symbols.iter().find(|s| s.name == n).map(|s| s.kind);
        assert_eq!(named("Shape"), Some(SymbolKind::Interface));
        assert_eq!(named("Widget"), Some(SymbolKind::Class));
        assert_eq!(named("Config"), Some(SymbolKind::Class)); // `object` singleton
    }

    // ---- qualified symbol identity (#248) ---------------------------------

    #[test]
    fn rust_records_the_qualifier_of_a_path_call() {
        let g = build_rust(r#"fn g() { Language::from_extension("rs"); }"#);
        let r = g
            .references
            .iter()
            .find(|r| r.name == "from_extension")
            .expect("call recorded");
        assert_eq!(r.qualifier.as_deref(), Some("Language"));
    }

    #[test]
    fn rust_records_the_last_path_segment_as_the_qualifier() {
        let g = build_rust("fn g() { a::b::c(); }");
        let r = g
            .references
            .iter()
            .find(|r| r.name == "c")
            .expect("call recorded");
        assert_eq!(r.qualifier.as_deref(), Some("b"));
    }

    #[test]
    fn rust_records_no_qualifier_for_a_bare_call() {
        let g = build_rust("fn g() { helper(); }");
        let r = g
            .references
            .iter()
            .find(|r| r.name == "helper")
            .expect("call recorded");
        assert_eq!(r.qualifier, None);
    }

    #[test]
    fn rust_never_records_a_receiver_name_as_a_qualifier() {
        // The receiver is a value, so `x` itself must never be the qualifier.
        // #251 types the receiver where the body says what it is, and the
        // qualifier is then the *type*; where it does not, nothing is recorded.
        let typed = build_rust("fn g(x: Thing) { x.helper(); }");
        let r = typed
            .references
            .iter()
            .find(|r| r.name == "helper")
            .expect("call recorded");
        assert_eq!(r.qualifier.as_deref(), Some("Thing"), "the type, not `x`");
        assert_eq!(r.kind, RefKind::Method);

        let untyped = build_rust("fn g() { let x = make(); x.helper(); }");
        let r = untyped
            .references
            .iter()
            .find(|r| r.name == "helper")
            .expect("call recorded");
        assert_eq!(r.qualifier, None);
        assert_eq!(r.kind, RefKind::Method);
    }

    #[test]
    fn rust_records_the_owning_type_of_a_method() {
        let g = build_rust("struct Widget; impl Widget { fn get(&self) {} }");
        let s = g
            .symbols
            .iter()
            .find(|s| s.name == "get")
            .expect("method recorded");
        assert_eq!(s.owner.as_deref(), Some("Widget"));
    }

    #[test]
    fn rust_records_no_owner_for_a_free_function() {
        let g = build_rust("fn helper() {}");
        let s = g
            .symbols
            .iter()
            .find(|s| s.name == "helper")
            .expect("fn recorded");
        assert_eq!(s.owner, None);
    }

    #[test]
    fn rust_records_an_inline_module_as_the_owner() {
        // `util::helper()` names the module, so the module has to be an owner
        // for the qualifier to match anything.
        let g = build_rust("mod util { pub fn helper() {} }");
        let s = g
            .symbols
            .iter()
            .find(|s| s.name == "helper")
            .expect("fn recorded");
        assert_eq!(s.owner.as_deref(), Some("util"));
    }

    #[test]
    fn python_records_the_owning_class_of_a_method() {
        let g = build(
            Language::Python,
            "class Widget:\n    def get(self):\n        pass\n",
        );
        let s = g
            .symbols
            .iter()
            .find(|s| s.name == "get")
            .expect("method recorded");
        assert_eq!(s.owner.as_deref(), Some("Widget"));
    }

    #[test]
    fn a_slice_with_no_qualifier_or_owner_serializes_unchanged() {
        // Both new fields are skipped when absent, so a file of plain free
        // functions keeps the byte-identical slice — and therefore the content
        // hash — it had before #248, exactly as `RefKind` did in #223.
        let g = build_rust("fn helper() -> u32 { 1 } fn main() { helper(); }");
        let json = serde_json::to_string(&g).expect("serialize");
        assert!(!json.contains("qualifier"), "{json}");
        assert!(!json.contains("owner"), "{json}");
    }

    #[test]
    fn rust_records_a_generic_impl_by_its_base_type() {
        // A call site writes `Holder::get()`, never `Holder<T>::get()`, so the
        // owner has to be the bare type name or the qualifier never matches.
        let g = build_rust("struct Holder<T>(T); impl<T> Holder<T> { fn get(&self) {} }");
        let s = g
            .symbols
            .iter()
            .find(|s| s.name == "get")
            .expect("method recorded");
        assert_eq!(s.owner.as_deref(), Some("Holder"));
    }

    #[test]
    fn rust_records_a_path_qualified_impl_by_its_last_segment() {
        let g = build_rust("impl crate::model::Widget { fn get(&self) {} }");
        let s = g
            .symbols
            .iter()
            .find(|s| s.name == "get")
            .expect("method recorded");
        assert_eq!(s.owner.as_deref(), Some("Widget"));
    }

    // ---- functions passed as values (#250) --------------------------------

    /// `(name, kind)` for every reference in `src`.
    fn rust_refs(src: &str) -> Vec<(String, RefKind)> {
        build_rust(src)
            .references
            .iter()
            .map(|r| (r.name.clone(), r.kind))
            .collect()
    }

    #[test]
    fn rust_records_a_function_passed_as_a_call_argument() {
        let refs = rust_refs("fn g(o: Opt) { o.and_then(helper); }");
        assert!(
            refs.contains(&("helper".to_string(), RefKind::Value)),
            "{refs:?}"
        );
    }

    #[test]
    fn rust_records_a_function_passed_to_a_free_call() {
        let refs = rust_refs("fn g() { register(handler); }");
        assert!(
            refs.contains(&("handler".to_string(), RefKind::Value)),
            "{refs:?}"
        );
    }

    #[test]
    fn a_called_function_is_not_also_recorded_as_a_value() {
        let refs = rust_refs("fn g() { helper(); }");
        assert_eq!(
            refs.iter().filter(|(n, _)| n == "helper").count(),
            1,
            "{refs:?}"
        );
        assert!(
            refs.contains(&("helper".to_string(), RefKind::Free)),
            "{refs:?}"
        );
    }

    #[test]
    fn a_literal_argument_records_nothing() {
        let refs = rust_refs("fn g() { take(1); }");
        assert_eq!(
            refs.iter().filter(|(n, _)| n != "take").count(),
            0,
            "{refs:?}"
        );
    }

    #[test]
    fn a_local_variable_passed_as_an_argument_is_recorded_too() {
        // Deliberate over-inclusion, pinned so it is a decision rather than a
        // surprise: extraction is per-file and cannot know whether `x` names a
        // local or a function. It costs a row and, for `unreferenced`, errs
        // towards *not* calling something dead — the safe direction for a tool
        // whose suggested action is deletion.
        let refs = rust_refs("fn g(x: u32) { take(x); }");
        assert!(
            refs.contains(&("x".to_string(), RefKind::Value)),
            "{refs:?}"
        );
    }

    // ---- syntactic receiver typing (#251) ---------------------------------

    /// The qualifier recorded on the reference named `name`.
    fn rust_qualifier_of(src: &str, name: &str) -> Option<String> {
        build_rust(src)
            .references
            .iter()
            .find(|r| r.name == name)
            .expect("reference recorded")
            .qualifier
            .clone()
    }

    #[test]
    fn a_receiver_bound_from_a_constructor_is_typed() {
        let q = rust_qualifier_of("fn g() { let w = Widget::new(); w.get(); }", "get");
        assert_eq!(q.as_deref(), Some("Widget"));
    }

    #[test]
    fn a_receiver_bound_from_a_struct_literal_is_typed() {
        let q = rust_qualifier_of("fn g() { let w = Widget { n: 1 }; w.get(); }", "get");
        assert_eq!(q.as_deref(), Some("Widget"));
    }

    #[test]
    fn a_receiver_from_a_typed_parameter_is_typed() {
        let q = rust_qualifier_of("fn g(w: Widget) { w.get(); }", "get");
        assert_eq!(q.as_deref(), Some("Widget"));
    }

    #[test]
    fn a_receiver_from_a_reference_parameter_is_typed() {
        // `&Widget` and `&mut Widget` are the common shapes; the type is what
        // matters, not how it is borrowed.
        assert_eq!(
            rust_qualifier_of("fn g(w: &Widget) { w.get(); }", "get").as_deref(),
            Some("Widget")
        );
        assert_eq!(
            rust_qualifier_of("fn g(w: &mut Widget) { w.get(); }", "get").as_deref(),
            Some("Widget")
        );
    }

    #[test]
    fn a_receiver_of_unknown_origin_stays_unqualified() {
        // `make()` says nothing about what it returns without a type system.
        let q = rust_qualifier_of("fn g() { let w = make(); w.get(); }", "get");
        assert_eq!(q, None);
    }

    #[test]
    fn a_generic_receiver_stays_unqualified() {
        // Explicitly out of scope: a type parameter names no type in the view,
        // and guessing an impl is the thing #223 stopped doing.
        let q = rust_qualifier_of("fn g<T: Tr>(w: T) { w.get(); }", "get");
        assert_eq!(q, None);
    }

    #[test]
    fn receiver_types_do_not_leak_between_functions() {
        // `w` means something different in each body; a flat table would carry
        // the first one into the second.
        let src = "fn a() { let w = Widget::new(); }\nfn b(w: Other) { w.get(); }";
        assert_eq!(rust_qualifier_of(src, "get").as_deref(), Some("Other"));
    }

    #[test]
    fn a_plain_method_call_is_still_a_method_reference() {
        // Typing the receiver must not turn it into a free call: the shape is
        // what stops a std method being credited to a project function (#223).
        let g = build_rust("fn g(w: Widget) { w.get(); }");
        let r = g.references.iter().find(|r| r.name == "get").unwrap();
        assert_eq!(r.kind, RefKind::Method);
    }

    #[test]
    fn a_self_receiver_takes_the_enclosing_type() {
        // `self` is the one receiver whose type is always written down: it is
        // the impl block the call sits in.
        let g = build_rust("impl Widget { fn get(&self) { self.helper(); } }");
        let r = g
            .references
            .iter()
            .find(|r| r.name == "helper")
            .expect("call recorded");
        assert_eq!(r.qualifier.as_deref(), Some("Widget"));
    }

    #[test]
    fn a_non_rust_method_call_records_no_receiver_type() {
        // Receiver typing is Rust-only for now; every other language behaves
        // exactly as it did rather than guessing (#251).
        let g = build(Language::Python, "def g(w):\n    w.get()\n");
        let r = g
            .references
            .iter()
            .find(|r| r.name == "get")
            .expect("call recorded");
        assert_eq!(r.qualifier, None);
    }

    // ---- imports (#252) ---------------------------------------------------

    /// `(name, path)` for every import in `src`.
    fn imports_of(language: Language, src: &str) -> Vec<(String, Vec<String>)> {
        build(language, src)
            .imports
            .iter()
            .map(|i| (i.name.clone(), i.path.clone()))
            .collect()
    }

    #[test]
    fn rust_records_a_simple_use_declaration() {
        let imports = imports_of(Language::Rust, "use crate::model::Symbol;");
        assert_eq!(
            imports,
            vec![("Symbol".to_string(), vec!["model".to_string()])],
            "`crate` is a relative marker, not a path component"
        );
    }

    #[test]
    fn rust_records_each_name_in_a_use_list() {
        let imports = imports_of(Language::Rust, "use a::b::{C, D};");
        assert_eq!(
            imports,
            vec![
                ("C".to_string(), vec!["a".to_string(), "b".to_string()]),
                ("D".to_string(), vec!["a".to_string(), "b".to_string()]),
            ]
        );
    }

    #[test]
    fn rust_records_an_aliased_import_under_its_local_name() {
        // `as` renames it for this file, and the local name is what a reference
        // in this file will be written as.
        let imports = imports_of(Language::Rust, "use a::Thing as Other;");
        assert_eq!(imports, vec![("Other".to_string(), vec!["a".to_string()])]);
    }

    #[test]
    fn rust_records_nothing_for_a_glob_import() {
        // `use a::*` brings in names the file never spells out, so there is no
        // name to key on.
        assert!(imports_of(Language::Rust, "use a::b::*;").is_empty());
    }

    #[test]
    fn python_records_a_from_import() {
        let imports = imports_of(Language::Python, "from pkg.model import Symbol\n");
        assert_eq!(
            imports,
            vec![(
                "Symbol".to_string(),
                vec!["pkg".to_string(), "model".to_string()]
            )]
        );
    }

    #[test]
    fn typescript_records_a_named_import() {
        let imports = imports_of(Language::TypeScript, "import { Symbol } from './model';\n");
        assert_eq!(
            imports,
            vec![("Symbol".to_string(), vec!["model".to_string()])],
            "a leading `.` is a relative marker, not a path component"
        );
    }

    #[test]
    fn a_slice_with_no_imports_serializes_unchanged() {
        let g = build_rust("fn helper() {}");
        let json = serde_json::to_string(&g).expect("serialize");
        assert!(!json.contains("imports"), "{json}");
    }

    #[test]
    fn typescript_drops_a_bare_path_alias_segment() {
        // Found by scanning a real Next.js app: `@/` is a tsconfig path alias
        // rooted at the project, so it names no directory. Left in, it added a
        // segment that could never match and the import matched nothing at all.
        let imports = imports_of(
            Language::TypeScript,
            "import { resetDefaults } from '@/lib/redux/slices/colors';\n",
        );
        assert_eq!(
            imports,
            vec![(
                "resetDefaults".to_string(),
                vec![
                    "lib".to_string(),
                    "redux".to_string(),
                    "slices".to_string(),
                    "colors".to_string()
                ]
            )]
        );
    }

    #[test]
    fn typescript_keeps_a_scoped_package_name() {
        // `@scope/pkg` is a package name, not an alias — only a *bare* `@` is
        // the alias marker, so the scope survives.
        let imports = imports_of(
            Language::TypeScript,
            "import { render } from '@testing-library/react';\n",
        );
        assert_eq!(
            imports,
            vec![(
                "render".to_string(),
                vec!["@testing-library".to_string(), "react".to_string()]
            )]
        );
    }

    // ---- Java / Kotlin imports (#260) --------------------------------------

    #[test]
    fn java_records_an_import_declaration() {
        let imports = imports_of(Language::Java, "import java.util.List;\n");
        assert_eq!(
            imports,
            vec![(
                "List".to_string(),
                vec!["java".to_string(), "util".to_string()]
            )]
        );
    }

    #[test]
    fn java_records_a_static_import_under_the_member_name() {
        // `import static a.b.C.d;` brings `d` into scope, not `C`.
        let imports = imports_of(
            Language::Java,
            "import static org.junit.Assert.assertEquals;\n",
        );
        assert_eq!(
            imports,
            vec![(
                "assertEquals".to_string(),
                vec!["org".to_string(), "junit".to_string(), "Assert".to_string()]
            )]
        );
    }

    #[test]
    fn java_records_nothing_for_a_wildcard_import() {
        assert!(imports_of(Language::Java, "import java.util.*;\n").is_empty());
        assert!(imports_of(Language::Java, "import static java.util.Map.*;\n").is_empty());
    }

    #[test]
    fn kotlin_records_an_import_header() {
        let imports = imports_of(Language::Kotlin, "import kotlin.collections.List\n");
        assert_eq!(
            imports,
            vec![(
                "List".to_string(),
                vec!["kotlin".to_string(), "collections".to_string()]
            )]
        );
    }

    #[test]
    fn kotlin_records_an_aliased_import_under_its_local_name() {
        let imports = imports_of(Language::Kotlin, "import a.b.Thing as Other\n");
        assert_eq!(
            imports,
            vec![("Other".to_string(), vec!["a".to_string(), "b".to_string()])]
        );
    }

    #[test]
    fn kotlin_records_nothing_for_a_wildcard_import() {
        assert!(imports_of(Language::Kotlin, "import a.b.*\n").is_empty());
    }

    // ---- functions bound to a name in JS/TS (#257) -------------------------

    /// Symbols of kind `Function` in `src`, with their owner.
    fn ts_functions(src: &str) -> Vec<(String, Option<String>)> {
        build(Language::TypeScript, src)
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .map(|s| (s.name.clone(), s.owner.clone()))
            .collect()
    }

    #[test]
    fn ts_still_records_a_plainly_bound_function() {
        // Regression guard: these already worked before #257 and must keep
        // working.
        assert_eq!(
            ts_functions("const handler = () => {};"),
            vec![("handler".to_string(), None)]
        );
        assert_eq!(
            ts_functions("const handler = function () {};"),
            vec![("handler".to_string(), None)]
        );
        assert_eq!(
            ts_functions("export const Component = async () => {};"),
            vec![("Component".to_string(), None)]
        );
        assert_eq!(
            ts_functions("class Widget { render = () => {}; }"),
            vec![("render".to_string(), Some("Widget".to_string()))]
        );
    }

    #[test]
    fn ts_records_a_function_wrapped_in_a_higher_order_call() {
        // The real defect: `export const C = React.memo(() => {})` is the
        // dominant React idiom, and gonzalo extracted *zero* symbols from files
        // written that way (#257).
        let fns = ts_functions("export const Controls = React.memo(() => {});");
        assert_eq!(fns, vec![("Controls".to_string(), None)]);
    }

    #[test]
    fn ts_records_a_function_through_two_wrappers() {
        let fns = ts_functions("const Input = memo(forwardRef(() => {}));");
        assert_eq!(fns, vec![("Input".to_string(), None)]);
    }

    #[test]
    fn a_binding_to_a_call_with_no_function_in_it_records_nothing() {
        assert!(ts_functions("const total = compute(1, 2);").is_empty());
        assert!(ts_functions("const client = createClient({ url });").is_empty());
    }

    #[test]
    fn a_call_inside_a_wrapped_component_records_it_as_the_caller() {
        let g = build(
            Language::TypeScript,
            "export const Controls = React.memo(() => { useAppDispatch(); });",
        );
        let call = g
            .references
            .iter()
            .find(|r| r.name == "useAppDispatch")
            .expect("call recorded");
        assert_eq!(call.from.as_deref(), Some("Controls"));
    }

    #[test]
    fn ts_records_an_object_property_arrow_function() {
        // The name lives in `key` here, not `name`.
        let fns = ts_functions("const api = { fetchAll: () => {} };");
        assert_eq!(fns, vec![("fetchAll".to_string(), None)]);
    }

    #[test]
    fn a_binding_that_is_not_a_function_is_not_recorded_as_one() {
        assert!(ts_functions("const count = 1;").is_empty());
        assert!(ts_functions("const items = [1, 2];").is_empty());
        assert!(ts_functions("const other = helper;").is_empty());
    }

    #[test]
    fn a_call_inside_a_bound_function_records_it_as_the_caller() {
        // The whole point: 72% of references in a real Next.js app had no
        // enclosing function, so they reached neither `callers` nor `impact`.
        let g = build(
            Language::TypeScript,
            "const boolToString = (b: boolean) => format(b);",
        );
        let call = g
            .references
            .iter()
            .find(|r| r.name == "format")
            .expect("call recorded");
        assert_eq!(call.from.as_deref(), Some("boolToString"));
    }

    #[test]
    fn a_nested_bound_function_takes_over_as_the_caller() {
        let g = build(
            Language::TypeScript,
            "const outer = () => { const inner = () => { deep(); }; shallow(); };",
        );
        let from = |name: &str| {
            g.references
                .iter()
                .find(|r| r.name == name)
                .and_then(|r| r.from.clone())
        };
        assert_eq!(from("deep").as_deref(), Some("inner"));
        assert_eq!(from("shallow").as_deref(), Some("outer"));
    }

    #[test]
    fn an_iterator_method_does_not_make_its_binding_a_function() {
        // The guard that keeps this from trading a missing answer for a wrong
        // one: `items.map(..)` returns an array. Treating `total` as a function
        // would attribute everything inside the lambda to `total` rather than
        // to the function actually containing it.
        assert!(ts_functions("const total = items.map((x) => f(x));").is_empty());
        assert!(ts_functions("const kept = list.filter((x) => keep(x));").is_empty());
    }

    #[test]
    fn a_lambda_in_an_iterator_call_keeps_the_real_enclosing_function() {
        let g = build(
            Language::TypeScript,
            "const render = () => { const rows = items.map((x) => cell(x)); };",
        );
        let call = g
            .references
            .iter()
            .find(|r| r.name == "cell")
            .expect("call recorded");
        assert_eq!(call.from.as_deref(), Some("render"));
    }

    // ---- relative import depth (#261) --------------------------------------

    /// `(name, path, depth)` for every import in `src`.
    fn imports_with_depth(language: Language, src: &str) -> Vec<(String, Vec<String>, usize)> {
        build(language, src)
            .imports
            .iter()
            .map(|i| (i.name.clone(), i.path.clone(), i.depth))
            .collect()
    }

    #[test]
    fn python_records_the_depth_of_a_relative_import() {
        // `..pkg.sub` is two dots deep: the referencing file's parent package,
        // then `pkg/sub` under it.
        let imports = imports_with_depth(Language::Python, "from ..pkg.sub import C\n");
        assert_eq!(
            imports,
            vec![(
                "C".to_string(),
                vec!["pkg".to_string(), "sub".to_string()],
                2
            )]
        );
    }

    #[test]
    fn python_records_a_bare_single_dot_import() {
        let imports = imports_with_depth(Language::Python, "from . import A\n");
        assert_eq!(imports, vec![("A".to_string(), vec![], 1)]);
    }

    #[test]
    fn an_absolute_python_import_records_no_depth() {
        let imports = imports_with_depth(Language::Python, "from pkg import D\n");
        assert_eq!(imports, vec![("D".to_string(), vec!["pkg".to_string()], 0)]);
    }

    #[test]
    fn a_slice_with_only_absolute_imports_serializes_without_depth() {
        let g = build(Language::Python, "from pkg import D\n");
        let json = serde_json::to_string(&g).expect("serialize");
        assert!(!json.contains("depth"), "{json}");
    }

    // ---- .h headers are C++ too (#266) -------------------------------------

    /// `(name, kind)` for every symbol in `src` under `language`.
    fn symbols_of(language: Language, src: &str) -> Vec<(String, SymbolKind)> {
        let mut out: Vec<(String, SymbolKind)> = build(language, src)
            .symbols
            .iter()
            .map(|s| (s.name.clone(), s.kind))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.as_str().cmp(b.1.as_str())));
        out
    }

    #[test]
    fn a_h_file_is_parsed_as_cpp() {
        assert_eq!(Language::from_extension("h"), Some(Language::Cpp));
    }

    #[test]
    fn a_scoped_enum_in_a_header_records_its_own_name() {
        // The bug, pinned: under the C grammar `enum class Color` recorded
        // `Color` as a *function* and invented a symbol named `class`. On a
        // real C++ project that produced 372 symbols called `class` across 133
        // files (#266).
        let header = Language::from_extension("h").expect("h is a source extension");
        let symbols = symbols_of(header, "enum class Color { Red, Green };");
        assert_eq!(symbols, vec![("Color".to_string(), SymbolKind::Enum)]);
    }

    #[test]
    fn a_header_records_no_keyword_shaped_symbols() {
        let header = Language::from_extension("h").expect("h is a source extension");
        let symbols = symbols_of(
            header,
            "enum class A {};\nenum struct B { x };\nenum class C : unsigned char { y };",
        );
        let names: Vec<&str> = symbols.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            !names
                .iter()
                .any(|n| matches!(*n, "class" | "struct" | "namespace" | "template")),
            "{names:?}"
        );
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[test]
    fn cpp_declarations_in_a_header_record_their_real_names() {
        let header = Language::from_extension("h").expect("h is a source extension");
        let symbols = symbols_of(
            header,
            "namespace engine { class Widget { public: void go(); }; }",
        );
        let names: Vec<&str> = symbols.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"engine"), "{names:?}");
        assert!(names.contains(&"Widget"), "{names:?}");
    }

    #[test]
    fn a_pure_c_header_extracts_the_same_symbols_under_either_grammar() {
        // The criterion that decides whether this fix is safe: C++ is a near
        // superset, but "near" has to be evidence rather than assertion. If
        // these ever diverge, the fix is to try C++ first and re-parse as C on
        // a tree with errors.
        let c_source = "\
#include <stdio.h>
typedef struct Point { int x; int y; } Point;
enum Color { RED, GREEN };
struct Opaque;
static int helper(int n) { return n + 1; }
int main(void) { return helper(1); }
typedef int (*Callback)(void *ctx);
";
        assert_eq!(
            symbols_of(Language::C, c_source),
            symbols_of(Language::Cpp, c_source),
            "the two grammars must agree on plain C"
        );
    }

    #[test]
    fn a_pure_c_header_records_the_same_references_under_either_grammar() {
        let c_source = "\
static int helper(int n) { return n + 1; }
int main(void) { return helper(1) + abs(-2); }
";
        let names = |language| {
            let mut out: Vec<String> = build(language, c_source)
                .references
                .iter()
                .map(|r| format!("{}:{:?}", r.name, r.kind))
                .collect();
            out.sort();
            out
        };
        assert_eq!(names(Language::C), names(Language::Cpp));
    }

    // ---- C/C++ includes (#267) ---------------------------------------------

    /// `(name, path)` for every import in `src` under `language`.
    fn includes_of(language: Language, src: &str) -> Vec<(String, Vec<String>)> {
        build(language, src)
            .imports
            .iter()
            .map(|i| (i.name.clone(), i.path.clone()))
            .collect()
    }

    #[test]
    fn c_records_a_quoted_include_as_a_path() {
        // An include names a *file*, not a name, so it carries no name — a
        // stronger signal than any package path, because the file is the thing
        // rather than a convention a path is assumed to mirror (#267).
        let imports = includes_of(Language::C, "#include \"engine/render/pipeline.h\"\n");
        assert_eq!(
            imports,
            vec![(
                String::new(),
                vec![
                    "engine".to_string(),
                    "render".to_string(),
                    "pipeline".to_string()
                ]
            )]
        );
    }

    #[test]
    fn an_include_is_marked_as_bringing_a_whole_file() {
        let g = build(Language::C, "#include \"a/b.h\"\n");
        assert!(g.imports[0].brings_whole_file());
        let py = build(Language::Python, "from pkg import thing\n");
        assert!(!py.imports[0].brings_whole_file());
    }

    #[test]
    fn c_records_an_angled_include() {
        // A system header matches nothing in the view, exactly as
        // `import java.io.File` does after #260 — recorded, and correctly inert.
        let imports = includes_of(Language::C, "#include <vector>\n");
        assert_eq!(imports, vec![(String::new(), vec!["vector".to_string()])]);
    }

    #[test]
    fn cpp_records_includes_the_same_way_as_c() {
        let src = "#include \"a/b.hpp\"\n#include <memory>\n";
        assert_eq!(
            includes_of(Language::C, src),
            includes_of(Language::Cpp, src)
        );
    }

    #[test]
    fn an_include_with_no_directory_records_its_stem() {
        let imports = includes_of(Language::C, "#include \"config.h\"\n");
        assert_eq!(imports, vec![(String::new(), vec!["config".to_string()])]);
    }

    // ---- CommonJS require (#269) -------------------------------------------

    #[test]
    fn js_records_a_plain_require() {
        let imports = imports_of(Language::JavaScript, "const fs = require('fs');\n");
        assert_eq!(imports, vec![("fs".to_string(), vec!["fs".to_string()])]);
    }

    #[test]
    fn js_records_each_destructured_require_name() {
        let imports = imports_of(
            Language::JavaScript,
            "const { getEnv, createLogger } = require('../util/env');\n",
        );
        assert_eq!(
            imports,
            vec![
                (
                    "getEnv".to_string(),
                    vec!["util".to_string(), "env".to_string()]
                ),
                (
                    "createLogger".to_string(),
                    vec!["util".to_string(), "env".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn js_records_a_renamed_destructured_require_under_its_local_name() {
        // `{ a: c }` means this file writes `c`, the same rule the ES `as`
        // alias already follows.
        let imports = imports_of(
            Language::JavaScript,
            "const { helper: localHelper } = require('./mod');\n",
        );
        assert_eq!(
            imports,
            vec![("localHelper".to_string(), vec!["mod".to_string()])]
        );
    }

    #[test]
    fn a_computed_require_records_nothing() {
        // No literal path means no module to key on, the same way a glob import
        // introduces no name.
        assert!(imports_of(Language::JavaScript, "const m = require(name);\n").is_empty());
        assert!(imports_of(Language::JavaScript, "const m = require(`./${x}`);\n").is_empty());
    }

    #[test]
    fn a_binding_that_is_not_a_require_records_nothing() {
        assert!(imports_of(Language::JavaScript, "const total = compute(1);\n").is_empty());
        assert!(imports_of(Language::JavaScript, "const n = 1;\n").is_empty());
    }

    #[test]
    fn typescript_records_a_require_too() {
        let imports = imports_of(Language::TypeScript, "const fs = require('node:fs');\n");
        assert_eq!(
            imports,
            vec![("fs".to_string(), vec!["node:fs".to_string()])]
        );
    }
}
