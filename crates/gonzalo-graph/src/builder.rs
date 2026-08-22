//! Build a [`CodeGraph`] from source using tree-sitter. Parsing is
//! language-parameterized ([`Language`]); Rust, Python, JavaScript,
//! TypeScript/TSX, Go, Java, C#, C, C++, Ruby, PHP, Bash, Kotlin, Swift, Lua,
//! Scala, and Elixir are supported, and a new grammar is a matter of adding its
//! node-kind mappings.

use crate::model::{CodeGraph, RefKind, Reference, Symbol, SymbolKind};
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
            "c" | "h" => Some(Self::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some(Self::Cpp),
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
            Self::JavaScript => js_item_kind(node),
            // TypeScript/TSX are a superset of JavaScript's declarations.
            Self::TypeScript | Self::Tsx => js_item_kind(node).or(match node_kind {
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
fn js_item_kind(node: Node<'_>) -> Option<SymbolKind> {
    match node.kind() {
        "function_declaration" | "generator_function_declaration" | "method_definition" => {
            Some(SymbolKind::Function)
        }
        "class_declaration" | "abstract_class_declaration" => Some(SymbolKind::Class),
        // `const foo = () => {}` / `const foo = function () {}` and class-field
        // `foo = () => {}`: a binding whose value is an arrow/function expression
        // is a named function. The name lives on the binding's `name` field.
        "variable_declarator" | "public_field_definition" => {
            match node.child_by_field_name("value").map(|v| v.kind()) {
                Some("arrow_function" | "function_expression") => Some(SymbolKind::Function),
                _ => None,
            }
        }
        _ => None,
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

    if let Some(kind) = language.item_kind(node, bytes)
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
            kind: language.callee_kind(node),
        });
    }

    // Calls the grammar hides inside an opaque macro-argument node (#216).
    for (name, line) in language.macro_arg_calls(node, bytes) {
        graph.references.push(Reference {
            name,
            from: enclosing.clone(),
            line,
            // A token-tree call is a bare `ident(` by construction (#216).
            kind: RefKind::Free,
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
}
