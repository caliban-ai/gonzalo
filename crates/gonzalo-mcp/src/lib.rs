//! MCP server exposing the gonzalo code graph to agents (EPIC D).
//!
//! [`GonzaloMcp`] implements rmcp's [`ServerHandler`] over a
//! [`Service`](gonzalo_server::Service): agents spawn the `gonzalo-mcp` binary
//! (stdio) and call tools that answer from the local store. It exposes a
//! view-independent `status` and `views` tools plus two families of code-graph
//! query, each taking a `(repo, view_id)` view selector:
//!
//! - **Discovery** — `views` lists the indexed `(repo, view_id)` pairs and
//!   `status` reports how many exist. A selector naming no indexed view is a
//!   tool *error* that lists the real ones, never an empty result: the two are
//!   otherwise indistinguishable, and an agent reads `[]` as "nothing calls
//!   this" rather than "you asked the wrong question" (#210).
//! - **Per-symbol** — `search`/`node`/`callers`/`callees`/`impact`/`explore`
//!   answer questions about a name the caller already has, and `diff` compares
//!   two views.
//! - **Whole-view** — `overview`/`top`/`list`/`unreferenced` answer questions
//!   *about the graph* (what is here, what is heavily referenced, what is
//!   ambiguous, what nothing calls) so a caller can orient without knowing a
//!   symbol name first. `unreferenced` is explicitly heuristic; its tool
//!   description carries the caveats.
//!
//! The tool logic lives in plain methods ([`GonzaloMcp::tools`],
//! [`GonzaloMcp::dispatch`]) so it is unit-testable without an rmcp
//! [`RequestContext`]; the trait methods are thin adapters over them.

use gonzalo_graph::{Ranking, SymbolFilter, SymbolKind};
use gonzalo_server::Service;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestMethod, CallToolRequestParams, CallToolResult, Content, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde::Serialize;
use serde_json::{Map, Value};
use std::sync::Arc;

/// An MCP server backed by a gonzalo [`Service`].
#[derive(Clone)]
pub struct GonzaloMcp {
    service: Service,
    root: String,
}

impl GonzaloMcp {
    /// Build a server over `service`, reporting `root` (the store location) in
    /// `status`.
    pub fn new(service: Service, root: impl Into<String>) -> Self {
        Self {
            service,
            root: root.into(),
        }
    }

    /// The backing service (used by the D1b graph tools).
    pub fn service(&self) -> &Service {
        &self.service
    }

    /// The tools this server advertises: a view-independent `status`, the
    /// per-symbol code-graph queries, and the whole-view aggregates — all the
    /// graph tools taking a `(repo, view_id)` view selector.
    pub fn tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "status",
                "Report that the gonzalo-mcp server is up and its configured store root.",
                status_schema(),
            ),
            Tool::new(
                "search",
                "Find where a symbol `name` is defined in a view. Returns located definitions.",
                view_query_schema(),
            ),
            Tool::new(
                "node",
                "Inspect a symbol: its definitions, its callers, and its callees in a view.",
                view_query_schema(),
            ),
            Tool::new(
                "callers",
                "List the enclosing functions that call `name` in a view.",
                view_query_schema(),
            ),
            Tool::new(
                "callees",
                "List the names called from within `name` in a view.",
                view_query_schema(),
            ),
            Tool::new(
                "impact",
                "List every symbol transitively affected if `name` changes (caller closure).",
                view_query_schema(),
            ),
            Tool::new(
                "explore",
                "List references to `name` in a view (with their paths), for navigating outward.",
                view_query_schema(),
            ),
            Tool::new(
                "diff",
                "Structural diff between two views of a repo: symbols and references added/removed \
                 going from `view_a` to `view_b`.",
                diff_schema(),
            ),
            Tool::new(
                "overview",
                "Summarize a whole view without needing a symbol name: file/symbol/reference \
                 counts, a breakdown by kind and language, and the largest files. Start here when \
                 orienting in an unfamiliar repo.",
                overview_schema(),
            ),
            Tool::new(
                "top",
                "Rank a view's symbols: `fan_in` (most referenced), `fan_out` (calls the most \
                 names), or `definitions` (defined in the most places — a score above 1 means the \
                 name is ambiguous and traversals through it are unreliable).",
                top_schema(),
            ),
            Tool::new(
                "list",
                "Enumerate a view's symbols, optionally filtered by path prefix, kind, and name \
                 substring. Answers \"what is in this crate\" rather than \"where is this name\".",
                list_schema(),
            ),
            Tool::new(
                "views",
                "List every indexed view as (repo, view_id) with its file count and the commit it \
                 was indexed at. Call this first: `repo` and `view_id` must match a view produced \
                 by `gonzalo index`, and this is the only way to discover the valid values. \
                 Compare `base_commit` against the checkout's HEAD to spot a stale view.",
                status_schema(),
            ),
            Tool::new(
                "unreferenced",
                "Symbols with no inbound reference — dead-code CANDIDATES, not dead code. This is \
                 a heuristic over a name-matched graph and it does produce false positives. A \
                 function used only as a value (higher-order usage, e.g. `map_err(be)`) is a path \
                 expression rather than a call, so it registers nothing and will be reported \
                 wrongly; and an unused name is hidden by any same-named symbol that is used. \
                 References from tests and from the symbol itself do count, so test-only and \
                 recursive-only functions are never reported. Confirm every hit against the \
                 source before acting on it.",
                unreferenced_schema(),
            ),
        ]
    }

    /// The `status` payload: server health, the configured store root, and how
    /// many views are indexed.
    ///
    /// The view count is the point (#210): `status` is the tool an agent reaches
    /// for to self-check, and reporting only `ok` meant a server pointed at an
    /// empty or wrong store looked perfectly healthy.
    pub async fn status_json(&self) -> serde_json::Value {
        match self.service.graph_views().await {
            Ok(views) => serde_json::json!({
                "status": "ok",
                "root": self.root,
                "views": views.len(),
            }),
            Err(e) => serde_json::json!({
                "status": "degraded",
                "root": self.root,
                "error": e.to_string(),
            }),
        }
    }

    /// Dispatch a tool call by name, independent of the rmcp transport so it can
    /// be unit-tested. Bad arguments and store errors surface as a tool error
    /// (`CallToolResult::error`); an unknown tool is a `method_not_found` error.
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Option<Map<String, Value>>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // `status` takes no view selector.
        if name == "status" {
            return Ok(CallToolResult::success(vec![Content::text(
                self.status_json().await.to_string(),
            )]));
        }

        // `views` is the discovery tool — it takes no selector by definition.
        if name == "views" {
            return self.result(self.service.graph_views().await);
        }

        // `diff` selects two views instead of one view + a name.
        if name == "diff" {
            let (repo, view_a, view_b) = match diff_args(&arguments) {
                Ok(t) => t,
                Err(msg) => return Ok(tool_error(msg)),
            };
            for view in [&view_a, &view_b] {
                if let Some(err) = self.unknown_view_error(&repo, view).await {
                    return Ok(err);
                }
            }
            return self.result(self.service.graph_diff(&repo, &view_a, &view_b).await);
        }

        // The aggregate tools select a view but take no symbol name.
        if matches!(name, "overview" | "top" | "list" | "unreferenced") {
            let (repo, view) = match selector_args(&arguments) {
                Ok(t) => t,
                Err(msg) => return Ok(tool_error(msg)),
            };
            if let Some(err) = self.unknown_view_error(&repo, &view).await {
                return Ok(err);
            }
            return self.aggregate(name, &repo, &view, &arguments).await;
        }

        // An unknown tool is `method_not_found` regardless of arguments — decided
        // before parsing the view selector so it isn't masked by a missing-arg
        // error.
        if !matches!(
            name,
            "search" | "node" | "callers" | "callees" | "impact" | "explore"
        ) {
            return Err(rmcp::ErrorData::method_not_found::<CallToolRequestMethod>());
        }

        // Every graph tool selects a view by (repo, view_id) and a `name`.
        let (repo, view, sym) = match view_args(&arguments) {
            Ok(t) => t,
            Err(msg) => return Ok(tool_error(msg)),
        };

        // An unresolvable selector is an error, never an empty result (#210):
        // otherwise a typo in `view_id` reads as "nothing calls this".
        if let Some(err) = self.unknown_view_error(&repo, &view).await {
            return Ok(err);
        }

        match name {
            "search" => self.result(self.service.graph_definitions(&repo, &view, &sym).await),
            "callers" => self.result(self.service.graph_callers_of(&repo, &view, &sym).await),
            "callees" => self.result(self.service.graph_callees(&repo, &view, &sym).await),
            "impact" => self.result(self.service.graph_impact(&repo, &view, &sym).await),
            "explore" => self.result(self.service.graph_references_to(&repo, &view, &sym).await),
            "node" => self.node(&repo, &view, &sym).await,
            // Unreachable: the known-tool guard above already returned for any
            // other name.
            _ => Err(rmcp::ErrorData::method_not_found::<CallToolRequestMethod>()),
        }
    }

    /// A tool error when `(repo, view_id)` names no indexed view, or `None` when
    /// the selector resolves.
    ///
    /// The message names the unresolved selector *and* lists the views that do
    /// exist, so a caller that guessed wrong can correct itself in one round
    /// trip instead of concluding the code is not there (#210).
    async fn unknown_view_error(&self, repo: &str, view: &str) -> Option<CallToolResult> {
        match self.service.view_exists(repo, view).await {
            Ok(true) => None,
            Ok(false) => {
                let known = match self.service.graph_views().await {
                    Ok(views) if !views.is_empty() => views
                        .iter()
                        .map(|v| format!("{}/{}", v.repo, v.view_id))
                        .collect::<Vec<_>>()
                        .join(", "),
                    Ok(_) => "none — run `gonzalo index` first".to_string(),
                    Err(e) => format!("<could not list views: {e}>"),
                };
                Some(tool_error(format!(
                    "no indexed view '{repo}/{view}'. This is a selector error, not an empty \
                     result. Indexed views: {known}. Call `views` to list them."
                )))
            }
            // A store failure is reported as itself rather than as "no view".
            Err(e) => Some(tool_error(e.to_string())),
        }
    }

    /// Turn a service query outcome into a tool result: JSON on success, a tool
    /// error carrying the message on failure.
    fn result<T: Serialize>(
        &self,
        outcome: gonzalo_core::Result<T>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match outcome {
            Ok(value) => Ok(success_json(&value)),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }

    /// Dispatch the view-wide aggregate tools, which take a `(repo, view_id)`
    /// selector plus their own optional arguments rather than a symbol name.
    async fn aggregate(
        &self,
        name: &str,
        repo: &str,
        view: &str,
        arguments: &Option<Map<String, Value>>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match name {
            "overview" => {
                let largest = match usize_arg(arguments, "largest", DEFAULT_TOP_LIMIT) {
                    Ok(n) => n,
                    Err(msg) => return Ok(tool_error(msg)),
                };
                self.result(self.service.graph_overview(repo, view, largest).await)
            }
            "top" => {
                let ranking = match ranking_arg(arguments) {
                    Ok(r) => r,
                    Err(msg) => return Ok(tool_error(msg)),
                };
                let limit = match usize_arg(arguments, "limit", DEFAULT_TOP_LIMIT) {
                    Ok(n) => n,
                    Err(msg) => return Ok(tool_error(msg)),
                };
                self.result(self.service.graph_top(repo, view, ranking, limit).await)
            }
            "list" => {
                let filter = match filter_args(arguments) {
                    Ok(f) => f,
                    Err(msg) => return Ok(tool_error(msg)),
                };
                let limit = match usize_arg(arguments, "limit", DEFAULT_LIST_LIMIT) {
                    Ok(n) => n,
                    Err(msg) => return Ok(tool_error(msg)),
                };
                self.result(self.service.graph_list(repo, view, &filter, limit).await)
            }
            "unreferenced" => {
                let filter = match filter_args(arguments) {
                    Ok(f) => f,
                    Err(msg) => return Ok(tool_error(msg)),
                };
                let exclude_tests = match bool_arg(arguments, "exclude_tests", true) {
                    Ok(b) => b,
                    Err(msg) => return Ok(tool_error(msg)),
                };
                let limit = match usize_arg(arguments, "limit", DEFAULT_LIST_LIMIT) {
                    Ok(n) => n,
                    Err(msg) => return Ok(tool_error(msg)),
                };
                self.result(
                    self.service
                        .graph_unreferenced(repo, view, &filter, exclude_tests, limit)
                        .await,
                )
            }
            // Unreachable: the caller already matched on these four names.
            _ => Err(rmcp::ErrorData::method_not_found::<CallToolRequestMethod>()),
        }
    }

    /// The `node` aggregate: definitions + callers + callees for one symbol.
    async fn node(
        &self,
        repo: &str,
        view: &str,
        sym: &str,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let defs = match self.service.graph_definitions(repo, view, sym).await {
            Ok(d) => d,
            Err(e) => return Ok(tool_error(e.to_string())),
        };
        let callers = match self.service.graph_callers_of(repo, view, sym).await {
            Ok(c) => c,
            Err(e) => return Ok(tool_error(e.to_string())),
        };
        let callees = match self.service.graph_callees(repo, view, sym).await {
            Ok(c) => c,
            Err(e) => return Ok(tool_error(e.to_string())),
        };
        let payload = serde_json::json!({
            "definitions": defs,
            "callers": callers,
            "callees": callees,
        });
        Ok(success_json(&payload))
    }
}

/// Input schema for the view queries: `repo`, `view_id`, and `name` — all
/// required strings.
fn view_query_schema() -> Arc<Map<String, Value>> {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "repo": {
                "type": "string",
                "description": "repository of an INDEXED view, e.g. acme/widgets — must match one \
                                reported by `views`, not an arbitrary name"
            },
            "view_id": {
                "type": "string",
                "description": "view id of an INDEXED view, e.g. main — must match one reported \
                                by `views`"
            },
            "name": { "type": "string", "description": "symbol name to query" }
        },
        "required": ["repo", "view_id", "name"],
        "additionalProperties": false
    });
    Arc::new(schema.as_object().expect("object schema").clone())
}

/// Input schema for `diff`: `repo`, `view_a`, and `view_b` — all required.
fn diff_schema() -> Arc<Map<String, Value>> {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "repo": {
                "type": "string",
                "description": "repository of two INDEXED views — must match `views` output"
            },
            "view_a": { "type": "string", "description": "the base view id (must be indexed)" },
            "view_b": {
                "type": "string",
                "description": "the view id to compare against the base (must be indexed)"
            }
        },
        "required": ["repo", "view_a", "view_b"],
        "additionalProperties": false
    });
    Arc::new(schema.as_object().expect("object schema").clone())
}

/// Default number of entries returned by `overview.largest_files` and `top`.
const DEFAULT_TOP_LIMIT: usize = 20;
/// Default number of symbols returned by `list`.
const DEFAULT_LIST_LIMIT: usize = 100;

/// Input schema for `overview`: a view selector plus an optional cap on the
/// `largest_files` listing.
fn overview_schema() -> Arc<Map<String, Value>> {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "repo": {
                "type": "string",
                "description": "repository of an INDEXED view, e.g. acme/widgets — must match one \
                                reported by `views`, not an arbitrary name"
            },
            "view_id": {
                "type": "string",
                "description": "view id of an INDEXED view, e.g. main — must match one reported \
                                by `views`"
            },
            "largest": {
                "type": "integer",
                "minimum": 0,
                "description": "how many of the largest files to list (default 20); the counts \
                                themselves are never truncated"
            }
        },
        "required": ["repo", "view_id"],
        "additionalProperties": false
    });
    Arc::new(schema.as_object().expect("object schema").clone())
}

/// Input schema for `top`: a view selector, the required ranking, and a limit.
fn top_schema() -> Arc<Map<String, Value>> {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "repo": {
                "type": "string",
                "description": "repository of an INDEXED view, e.g. acme/widgets — must match one \
                                reported by `views`, not an arbitrary name"
            },
            "view_id": {
                "type": "string",
                "description": "view id of an INDEXED view, e.g. main — must match one reported \
                                by `views`"
            },
            "by": {
                "type": "string",
                "enum": ["fan_in", "fan_out", "definitions"],
                "description": "fan_in = most referenced; fan_out = calls the most distinct \
                                names; definitions = defined in the most paths (>1 is ambiguous)"
            },
            "limit": {
                "type": "integer",
                "minimum": 0,
                "description": "maximum entries to return (default 20)"
            }
        },
        "required": ["repo", "view_id", "by"],
        "additionalProperties": false
    });
    Arc::new(schema.as_object().expect("object schema").clone())
}

/// Input schema for `list`: a view selector plus conjunctive filters.
fn list_schema() -> Arc<Map<String, Value>> {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "repo": {
                "type": "string",
                "description": "repository of an INDEXED view, e.g. acme/widgets — must match one \
                                reported by `views`, not an arbitrary name"
            },
            "view_id": {
                "type": "string",
                "description": "view id of an INDEXED view, e.g. main — must match one reported \
                                by `views`"
            },
            "path_prefix": {
                "type": "string",
                "description": "only symbols whose path starts with this (scopes to a crate or \
                                directory)"
            },
            "kind": {
                "type": "string",
                "enum": [
                    "function", "struct", "enum", "trait", "impl", "module",
                    "const", "static", "type_alias", "class", "interface"
                ],
                "description": "only symbols of this kind"
            },
            "name_contains": {
                "type": "string",
                "description": "only symbols whose name contains this substring"
            },
            "limit": {
                "type": "integer",
                "minimum": 0,
                "description": "maximum symbols to return (default 100)"
            }
        },
        "required": ["repo", "view_id"],
        "additionalProperties": false
    });
    Arc::new(schema.as_object().expect("object schema").clone())
}

/// Input schema for `unreferenced`: the `list` filters plus the test-scope
/// toggle.
fn unreferenced_schema() -> Arc<Map<String, Value>> {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "repo": {
                "type": "string",
                "description": "repository of an INDEXED view, e.g. acme/widgets — must match one \
                                reported by `views`, not an arbitrary name"
            },
            "view_id": {
                "type": "string",
                "description": "view id of an INDEXED view, e.g. main — must match one reported \
                                by `views`"
            },
            "path_prefix": {
                "type": "string",
                "description": "only symbols whose path starts with this (scopes to a crate or \
                                directory)"
            },
            "kind": {
                "type": "string",
                "enum": [
                    "function", "struct", "enum", "trait", "impl", "module",
                    "const", "static", "type_alias", "class", "interface"
                ],
                "description": "only symbols of this kind; `function` is usually what you want"
            },
            "name_contains": {
                "type": "string",
                "description": "only symbols whose name contains this substring"
            },
            "exclude_tests": {
                "type": "boolean",
                "description": "drop symbols inside a `mod tests`/`mod test` block or under a \
                                tests/ directory (default true); without it the result is mostly \
                                test helpers"
            },
            "limit": {
                "type": "integer",
                "minimum": 0,
                "description": "maximum candidates to return (default 100)"
            }
        },
        "required": ["repo", "view_id"],
        "additionalProperties": false
    });
    Arc::new(schema.as_object().expect("object schema").clone())
}

/// A minimal object schema for the argument-free `status` tool.
fn status_schema() -> Arc<Map<String, Value>> {
    let schema = serde_json::json!({ "type": "object", "additionalProperties": false });
    Arc::new(schema.as_object().expect("object schema").clone())
}

/// Extract a required string argument by key.
fn str_arg(arguments: &Option<Map<String, Value>>, key: &str) -> Result<String, String> {
    arguments
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing required string argument '{key}'"))
}

/// Extract the required `(repo, view_id, name)` view selector from tool args.
fn view_args(arguments: &Option<Map<String, Value>>) -> Result<(String, String, String), String> {
    Ok((
        str_arg(arguments, "repo")?,
        str_arg(arguments, "view_id")?,
        str_arg(arguments, "name")?,
    ))
}

/// Extract the required `(repo, view_id)` selector, for tools that address a
/// whole view rather than a symbol in it.
fn selector_args(arguments: &Option<Map<String, Value>>) -> Result<(String, String), String> {
    Ok((str_arg(arguments, "repo")?, str_arg(arguments, "view_id")?))
}

/// Extract an optional non-negative integer argument, or `default` if absent.
fn usize_arg(
    arguments: &Option<Map<String, Value>>,
    key: &str,
    default: usize,
) -> Result<usize, String> {
    match arguments.as_ref().and_then(|m| m.get(key)) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| format!("argument '{key}' must be a non-negative integer, got {v}")),
    }
}

/// Extract an optional boolean argument, or `default` if absent.
fn bool_arg(
    arguments: &Option<Map<String, Value>>,
    key: &str,
    default: bool,
) -> Result<bool, String> {
    match arguments.as_ref().and_then(|m| m.get(key)) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(v) => Err(format!("argument '{key}' must be a boolean, got {v}")),
    }
}

/// Extract the required `by` ranking for `top`.
fn ranking_arg(arguments: &Option<Map<String, Value>>) -> Result<Ranking, String> {
    let raw = str_arg(arguments, "by")?;
    match raw.as_str() {
        "fan_in" => Ok(Ranking::FanIn),
        "fan_out" => Ok(Ranking::FanOut),
        "definitions" => Ok(Ranking::Definitions),
        other => Err(format!(
            "unknown ranking '{other}': expected one of fan_in, fan_out, definitions"
        )),
    }
}

/// Build the `list` filter from its optional arguments.
fn filter_args(arguments: &Option<Map<String, Value>>) -> Result<SymbolFilter, String> {
    let mut filter = SymbolFilter::default();
    if let Some(prefix) = opt_str_arg(arguments, "path_prefix")? {
        filter = filter.path_prefix(prefix);
    }
    if let Some(needle) = opt_str_arg(arguments, "name_contains")? {
        filter = filter.name_contains(needle);
    }
    if let Some(raw) = opt_str_arg(arguments, "kind")? {
        filter = filter.kind(parse_kind(&raw)?);
    }
    Ok(filter)
}

/// Extract an optional string argument, erroring if present but not a string.
fn opt_str_arg(
    arguments: &Option<Map<String, Value>>,
    key: &str,
) -> Result<Option<String>, String> {
    match arguments.as_ref().and_then(|m| m.get(key)) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(v) => Err(format!("argument '{key}' must be a string, got {v}")),
    }
}

/// Parse a [`SymbolKind`] from its lowercase wire name.
fn parse_kind(raw: &str) -> Result<SymbolKind, String> {
    [
        SymbolKind::Function,
        SymbolKind::Struct,
        SymbolKind::Enum,
        SymbolKind::Trait,
        SymbolKind::Impl,
        SymbolKind::Module,
        SymbolKind::Const,
        SymbolKind::Static,
        SymbolKind::TypeAlias,
        SymbolKind::Class,
        SymbolKind::Interface,
    ]
    .into_iter()
    .find(|k| k.as_str() == raw)
    .ok_or_else(|| format!("unknown kind '{raw}'"))
}

/// Extract the required `(repo, view_a, view_b)` selector for `diff`.
fn diff_args(arguments: &Option<Map<String, Value>>) -> Result<(String, String, String), String> {
    Ok((
        str_arg(arguments, "repo")?,
        str_arg(arguments, "view_a")?,
        str_arg(arguments, "view_b")?,
    ))
}

/// A tool error carrying `msg` as text (`isError = true`).
fn tool_error(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg.into())])
}

/// A successful tool result whose single text block is `value` as JSON.
fn success_json<T: Serialize>(value: &T) -> CallToolResult {
    match serde_json::to_string(value) {
        Ok(json) => CallToolResult::success(vec![Content::text(json)]),
        Err(e) => tool_error(format!("failed to serialize result: {e}")),
    }
}

impl ServerHandler for GonzaloMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("gonzalo-mcp: code-graph queries over a gonzalo view")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult::with_all_items(Self::tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.dispatch(request.name.as_ref(), request.arguments)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_core::{
        BlobStore, Identity, Manifest, Meta, PutResult, Record, RecordKind, Revision, Store,
    };
    use gonzalo_graph::build_rust;
    use gonzalo_store_fs::FsStore;
    use std::collections::BTreeMap;

    fn server(root: &str) -> GonzaloMcp {
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        GonzaloMcp::new(Service::new(fs.clone(), fs), root)
    }

    /// A server whose store already holds view `r`/`main` with two slices.
    async fn seeded_server() -> GonzaloMcp {
        seeded_with(&[
            ("lib.rs", "fn helper() {}"),
            ("main.rs", "fn main() { helper(); }"),
        ])
        .await
    }

    /// A server whose store holds view `r`/`main` assembled from `slices`.
    async fn seeded_with(slices: &[(&str, &str)]) -> GonzaloMcp {
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        let mut manifest = Manifest::new();
        for &(path, src) in slices {
            let hash = fs
                .put_blob(&build_rust(src).to_slice_bytes())
                .await
                .unwrap();
            manifest.insert(path, hash);
        }
        let body = manifest.to_body();
        let record = Record {
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            kind: RecordKind::GraphManifest,
            meta: Meta {
                author: Identity::new("tester"),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            key: Manifest::key("r", "main"),
        };
        assert!(matches!(
            fs.put(record, None).await.unwrap(),
            PutResult::Committed(_)
        ));
        GonzaloMcp::new(Service::new(fs.clone(), fs), "test-root")
    }

    fn args(repo: &str, view: &str, name: &str) -> Option<Map<String, Value>> {
        Some(
            serde_json::json!({ "repo": repo, "view_id": view, "name": name })
                .as_object()
                .unwrap()
                .clone(),
        )
    }

    /// The text of a result's first content block (via the serialized result).
    fn result_text(result: &CallToolResult) -> String {
        let v = serde_json::to_value(result).unwrap();
        v["content"][0]["text"]
            .as_str()
            .expect("text content")
            .to_string()
    }

    #[test]
    fn advertises_status_and_the_graph_tools() {
        let names: Vec<String> = GonzaloMcp::tools()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "status",
                "search",
                "node",
                "callers",
                "callees",
                "impact",
                "explore",
                "diff",
                "overview",
                "top",
                "list",
                "views",
                "unreferenced",
            ]
        );
    }

    // ---- aggregate / structural tools (#214) ------------------------------

    /// `(repo, view_id)` plus whatever extra arguments a tool takes.
    fn view_args_with(repo: &str, view: &str, extra: Value) -> Option<Map<String, Value>> {
        let mut map = serde_json::json!({ "repo": repo, "view_id": view })
            .as_object()
            .unwrap()
            .clone();
        for (k, v) in extra.as_object().expect("object") {
            map.insert(k.clone(), v.clone());
        }
        Some(map)
    }

    async fn call(tool: &str, extra: Value) -> Value {
        let s = seeded_server().await;
        let result = s
            .dispatch(tool, view_args_with("r", "main", extra))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "tool {tool} errored");
        serde_json::from_str(&result_text(&result)).unwrap()
    }

    #[tokio::test]
    async fn overview_tool_reports_the_shape_of_the_view() {
        // The seeded view is lib.rs (`helper`) + main.rs (`main` calling helper).
        let v = call("overview", serde_json::json!({})).await;
        assert_eq!(v["files"], 2);
        assert_eq!(v["symbols"], 2);
        assert_eq!(v["references"], 1);
        assert_eq!(v["by_kind"]["function"], 2);
        assert_eq!(v["by_language"]["rust"], 2);
        assert_eq!(v["largest_files"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn overview_bounds_the_largest_files_listing() {
        let v = call("overview", serde_json::json!({ "largest": 1 })).await;
        assert_eq!(v["largest_files"].as_array().unwrap().len(), 1);
        assert_eq!(v["files"], 2, "the count itself is not truncated");
    }

    #[tokio::test]
    async fn top_tool_ranks_by_fan_in() {
        let v = call("top", serde_json::json!({ "by": "fan_in" })).await;
        let helper = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "helper")
            .expect("helper is referenced once");
        assert_eq!(helper["score"], 1);
        assert_eq!(helper["paths"], serde_json::json!(["lib.rs"]));
    }

    #[tokio::test]
    async fn top_tool_ranks_by_definitions_for_the_ambiguity_report() {
        let v = call("top", serde_json::json!({ "by": "definitions" })).await;
        // Nothing is ambiguous in this view: every name has exactly one home.
        assert!(
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["score"] == 1)
        );
    }

    #[tokio::test]
    async fn top_tool_reports_truncation() {
        // Two names are defined in the seeded view (`helper`, `main`), so a
        // limit of 1 must report that something was left out.
        let v = call(
            "top",
            serde_json::json!({ "by": "definitions", "limit": 1 }),
        )
        .await;
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["total"], 2);
        assert_eq!(v["truncated"], true);
    }

    #[tokio::test]
    async fn top_tool_rejects_an_unknown_ranking() {
        let s = seeded_server().await;
        let result = s
            .dispatch(
                "top",
                view_args_with("r", "main", serde_json::json!({ "by": "sideways" })),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(result_text(&result).contains("sideways"));
    }

    #[tokio::test]
    async fn top_tool_requires_the_ranking_argument() {
        let s = seeded_server().await;
        let result = s
            .dispatch("top", view_args_with("r", "main", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(result_text(&result).contains("by"));
    }

    #[tokio::test]
    async fn list_tool_filters_by_kind() {
        let v = call("list", serde_json::json!({ "kind": "function" })).await;
        assert_eq!(v["total"], 2);
        assert!(
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|l| l["item"]["kind"] == "function")
        );
    }

    #[tokio::test]
    async fn list_tool_filters_by_path_prefix() {
        let v = call("list", serde_json::json!({ "path_prefix": "lib" })).await;
        assert_eq!(v["total"], 1);
        assert_eq!(v["items"][0]["path"], "lib.rs");
    }

    #[tokio::test]
    async fn list_tool_reports_truncation() {
        let v = call("list", serde_json::json!({ "limit": 1 })).await;
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["total"], 2);
    }

    #[tokio::test]
    async fn list_tool_rejects_an_unknown_kind() {
        let s = seeded_server().await;
        let result = s
            .dispatch(
                "list",
                view_args_with("r", "main", serde_json::json!({ "kind": "gizmo" })),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(result_text(&result).contains("gizmo"));
    }

    // ---- unknown view vs empty result (#210) -------------------------------

    /// Every tool that takes a view selector, with valid arguments except the
    /// view id.
    const VIEW_SCOPED: &[&str] = &[
        "search",
        "node",
        "callers",
        "callees",
        "impact",
        "explore",
        "overview",
        "top",
        "list",
        "unreferenced",
    ];

    fn args_for(tool: &str, repo: &str, view: &str) -> Option<Map<String, Value>> {
        let mut m = serde_json::json!({ "repo": repo, "view_id": view })
            .as_object()
            .unwrap()
            .clone();
        // Per-tool required extras.
        if matches!(
            tool,
            "search" | "node" | "callers" | "callees" | "impact" | "explore"
        ) {
            m.insert("name".into(), Value::String("helper".into()));
        }
        if tool == "top" {
            m.insert("by".into(), Value::String("fan_in".into()));
        }
        Some(m)
    }

    #[tokio::test]
    async fn an_unknown_view_id_is_an_error_not_an_empty_result() {
        let s = seeded_server().await;
        for tool in VIEW_SCOPED {
            let result = s.dispatch(tool, args_for(tool, "r", "mian")).await.unwrap();
            assert_eq!(result.is_error, Some(true), "{tool} must reject 'mian'");
            let text = result_text(&result);
            assert!(
                text.contains("r/mian"),
                "{tool} must name the selector: {text}"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_repo_is_an_error_not_an_empty_result() {
        let s = seeded_server().await;
        for tool in VIEW_SCOPED {
            let result = s
                .dispatch(tool, args_for(tool, "does-not-exist", "main"))
                .await
                .unwrap();
            assert_eq!(result.is_error, Some(true), "{tool} must reject the repo");
        }
    }

    #[tokio::test]
    async fn an_unknown_view_error_lists_the_views_that_exist() {
        let s = seeded_server().await;
        let result = s
            .dispatch("search", args_for("search", "r", "mian"))
            .await
            .unwrap();
        let text = result_text(&result);
        assert!(
            text.contains("r/main"),
            "must point at the real view: {text}"
        );
        assert!(
            text.contains("views"),
            "must name the discovery tool: {text}"
        );
    }

    #[tokio::test]
    async fn an_absent_symbol_in_a_valid_view_is_still_an_empty_result() {
        // The other half of #210: the two cases must be distinguishable, so a
        // genuine miss must NOT become an error.
        let s = seeded_server().await;
        let result = s
            .dispatch("search", args_for("search", "r", "main"))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));

        let result = s
            .dispatch(
                "callers",
                Some(
                    serde_json::json!({ "repo": "r", "view_id": "main", "name": "nonexistent" })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "a real miss is not an error");
        assert_eq!(
            serde_json::from_str::<Value>(&result_text(&result)).unwrap(),
            serde_json::json!([])
        );
    }

    #[tokio::test]
    async fn diff_rejects_an_unknown_view_on_either_side() {
        let s = seeded_server().await;
        for (a, b) in [("main", "nope"), ("nope", "main")] {
            let result = s
                .dispatch(
                    "diff",
                    Some(
                        serde_json::json!({ "repo": "r", "view_a": a, "view_b": b })
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(result.is_error, Some(true), "diff {a}->{b}");
            assert!(result_text(&result).contains("r/nope"));
        }
    }

    #[tokio::test]
    async fn views_tool_lists_indexed_views() {
        let s = seeded_server().await;
        let result = s.dispatch("views", None).await.unwrap();
        assert_ne!(result.is_error, Some(true));
        let v: Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["repo"], "r");
        assert_eq!(v[0]["view_id"], "main");
        assert_eq!(v[0]["files"], 2, "lib.rs + main.rs");
    }

    #[tokio::test]
    async fn views_tool_is_empty_on_a_fresh_store() {
        let s = server("test-root");
        let result = s.dispatch("views", None).await.unwrap();
        let v: Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_reports_the_number_of_indexed_views() {
        let s = seeded_server().await;
        let v: Value =
            serde_json::from_str(&result_text(&s.dispatch("status", None).await.unwrap())).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["views"], 1, "an empty store must be distinguishable");

        let empty = server("test-root");
        let v: Value =
            serde_json::from_str(&result_text(&empty.dispatch("status", None).await.unwrap()))
                .unwrap();
        assert_eq!(v["views"], 0);
    }

    #[tokio::test]
    async fn aggregate_tools_require_a_view_selector() {
        let s = seeded_server().await;
        for tool in ["overview", "top", "list", "unreferenced"] {
            let result = s.dispatch(tool, None).await.unwrap();
            assert_eq!(result.is_error, Some(true), "{tool} should reject no args");
            assert!(result_text(&result).contains("repo"));
        }
    }

    /// A view with a used function, an unused one, and a `mod tests` block.
    const DEAD_SRC: &str = "fn helper() {}\n\
                            fn main() { helper(); }\n\
                            fn orphan() {}\n\
                            #[cfg(test)]\n\
                            mod tests {\n    \
                                fn t_only() {}\n\
                            }\n";

    async fn call_on(server: &GonzaloMcp, tool: &str, extra: Value) -> Value {
        let result = server
            .dispatch(tool, view_args_with("r", "main", extra))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "tool {tool} errored");
        serde_json::from_str(&result_text(&result)).unwrap()
    }

    fn item_names(v: &Value) -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["item"]["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn unreferenced_tool_reports_only_uncalled_symbols() {
        let v = call("unreferenced", serde_json::json!({})).await;
        // The seeded view is `helper` (called from main) + `main` (called by
        // nothing), so only `main` is a candidate.
        assert_eq!(item_names(&v), vec!["main"]);
        assert_eq!(v["items"][0]["path"], "main.rs");
    }

    #[tokio::test]
    async fn unreferenced_tool_excludes_test_scopes_by_default() {
        let s = seeded_with(&[("lib.rs", DEAD_SRC)]).await;
        let v = call_on(&s, "unreferenced", serde_json::json!({})).await;
        let names = item_names(&v);
        assert!(names.contains(&"orphan".to_string()));
        assert!(!names.contains(&"t_only".to_string()));
    }

    #[tokio::test]
    async fn unreferenced_tool_can_include_test_scopes() {
        let s = seeded_with(&[("lib.rs", DEAD_SRC)]).await;
        let v = call_on(
            &s,
            "unreferenced",
            serde_json::json!({ "exclude_tests": false }),
        )
        .await;
        assert!(item_names(&v).contains(&"t_only".to_string()));
    }

    #[tokio::test]
    async fn unreferenced_tool_applies_the_symbol_filter() {
        let s = seeded_with(&[("lib.rs", DEAD_SRC)]).await;
        let v = call_on(&s, "unreferenced", serde_json::json!({ "kind": "struct" })).await;
        assert_eq!(v["total"], 0);
    }

    #[tokio::test]
    async fn unreferenced_tool_reports_truncation() {
        let s = seeded_with(&[("lib.rs", "fn a() {} fn b() {} fn c() {}")]).await;
        let v = call_on(&s, "unreferenced", serde_json::json!({ "limit": 2 })).await;
        assert_eq!(v["items"].as_array().unwrap().len(), 2);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["total"], 3);
    }

    #[tokio::test]
    async fn unreferenced_tool_rejects_a_non_boolean_exclude_tests() {
        let s = seeded_server().await;
        let result = s
            .dispatch(
                "unreferenced",
                view_args_with("r", "main", serde_json::json!({ "exclude_tests": "yes" })),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(result_text(&result).contains("exclude_tests"));
    }

    #[test]
    fn unreferenced_tool_description_states_the_heuristic_blind_spot() {
        let tool = GonzaloMcp::tools()
            .into_iter()
            .find(|t| t.name == "unreferenced")
            .expect("unreferenced is advertised");
        let description = tool.description.as_deref().unwrap_or_default();
        assert!(
            description.contains("heuristic"),
            "must not present candidates as proof: {description}"
        );
        assert!(
            description.contains("higher-order"),
            "must name the higher-order-usage blind spot: {description}"
        );
    }

    #[tokio::test]
    async fn diff_tool_reports_changes_between_two_views() {
        // Seed two views of `r` sharing the store, differing by one symbol.
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        for (view, src) in [
            ("v1", "fn keep() {}\nfn gone() {}"),
            ("v2", "fn keep() {}\nfn fresh() {}"),
        ] {
            let hash = fs
                .put_blob(&build_rust(src).to_slice_bytes())
                .await
                .unwrap();
            let mut manifest = Manifest::new();
            manifest.insert("lib.rs", hash);
            let body = manifest.to_body();
            let record = Record {
                revision: Revision::initial(body.bytes()),
                parent: None,
                body,
                kind: RecordKind::GraphManifest,
                meta: Meta {
                    author: Identity::new("t"),
                    origin_system: "t".into(),
                    created: 0,
                    updated: 0,
                    labels: BTreeMap::new(),
                },
                links: Vec::new(),
                key: Manifest::key("r", view),
            };
            assert!(matches!(
                fs.put(record, None).await.unwrap(),
                PutResult::Committed(_)
            ));
        }
        let mcp = GonzaloMcp::new(Service::new(fs.clone(), fs), "test-root");

        let diff_args = Some(
            serde_json::json!({ "repo": "r", "view_a": "v1", "view_b": "v2" })
                .as_object()
                .unwrap()
                .clone(),
        );
        let result = mcp.dispatch("diff", diff_args).await.unwrap();
        let diff: Value = serde_json::from_str(&result_text(&result)).unwrap();
        let added: Vec<&str> = diff["added_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["item"]["name"].as_str().unwrap())
            .collect();
        assert!(added.contains(&"fresh"));
        let removed: Vec<&str> = diff["removed_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["item"]["name"].as_str().unwrap())
            .collect();
        assert!(removed.contains(&"gone"));
    }

    #[tokio::test]
    async fn status_reports_ok_and_configured_root() {
        let s = server("/tmp/some-root");
        let payload = s.status_json().await;
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["root"], "/tmp/some-root");
    }

    #[tokio::test]
    async fn search_returns_located_definitions() {
        let s = seeded_server().await;
        let result = s
            .dispatch("search", args("r", "main", "helper"))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let defs: Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(defs[0]["path"], "lib.rs");
        assert_eq!(defs[0]["item"]["name"], "helper");
    }

    #[tokio::test]
    async fn impact_and_callees_return_names() {
        let s = seeded_server().await;
        let impact = s
            .dispatch("impact", args("r", "main", "helper"))
            .await
            .unwrap();
        let names: Value = serde_json::from_str(&result_text(&impact)).unwrap();
        assert_eq!(names, serde_json::json!(["main"]));

        let callees = s
            .dispatch("callees", args("r", "main", "main"))
            .await
            .unwrap();
        let names: Value = serde_json::from_str(&result_text(&callees)).unwrap();
        assert_eq!(names, serde_json::json!(["helper"]));
    }

    #[tokio::test]
    async fn node_aggregates_definitions_callers_and_callees() {
        let s = seeded_server().await;
        let result = s
            .dispatch("node", args("r", "main", "helper"))
            .await
            .unwrap();
        let node: Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(node["definitions"][0]["path"], "lib.rs");
        assert_eq!(node["callers"], serde_json::json!(["main"]));
        assert_eq!(node["callees"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn missing_arguments_are_a_tool_error() {
        let s = seeded_server().await;
        // No `name` provided.
        let bad = Some(
            serde_json::json!({ "repo": "r", "view_id": "main" })
                .as_object()
                .unwrap()
                .clone(),
        );
        let result = s.dispatch("search", bad).await.unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(result_text(&result).contains("name"));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_is_method_not_found() {
        let s = server("/r");
        assert!(s.dispatch("no_such_tool", None).await.is_err());
    }
}
