//! MCP server exposing the gonzalo code graph to agents (EPIC D).
//!
//! [`GonzaloMcp`] implements rmcp's [`ServerHandler`] over a
//! [`Service`](gonzalo_server::Service): agents spawn the `gonzalo-mcp` binary
//! (stdio) and call tools that answer from the local store. It exposes a
//! view-independent `status` tool plus two families of code-graph query, each
//! taking a `(repo, view_id)` view selector:
//!
//! - **Per-symbol** — `search`/`node`/`callers`/`callees`/`impact`/`explore`
//!   answer questions about a name the caller already has, and `diff` compares
//!   two views.
//! - **Whole-view** — `overview`/`top`/`list` answer questions *about the graph*
//!   (what is here, what is heavily referenced, what is ambiguous) so a caller
//!   can orient without knowing a symbol name first.
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
        ]
    }

    /// The `status` payload: server health plus the configured store root.
    pub fn status_json(&self) -> serde_json::Value {
        serde_json::json!({ "status": "ok", "root": self.root })
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
                self.status_json().to_string(),
            )]));
        }

        // `diff` selects two views instead of one view + a name.
        if name == "diff" {
            let (repo, view_a, view_b) = match diff_args(&arguments) {
                Ok(t) => t,
                Err(msg) => return Ok(tool_error(msg)),
            };
            return self.result(self.service.graph_diff(&repo, &view_a, &view_b).await);
        }

        // The aggregate tools select a view but take no symbol name.
        if matches!(name, "overview" | "top" | "list") {
            let (repo, view) = match selector_args(&arguments) {
                Ok(t) => t,
                Err(msg) => return Ok(tool_error(msg)),
            };
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
            // Unreachable: the caller already matched on these three names.
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
            "repo": { "type": "string", "description": "repository identifier, e.g. acme/widgets" },
            "view_id": { "type": "string", "description": "view id, e.g. main" },
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
            "repo": { "type": "string", "description": "repository identifier" },
            "view_a": { "type": "string", "description": "the base view id" },
            "view_b": { "type": "string", "description": "the view id to compare against the base" }
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
            "repo": { "type": "string", "description": "repository identifier, e.g. acme/widgets" },
            "view_id": { "type": "string", "description": "view id, e.g. main" },
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
            "repo": { "type": "string", "description": "repository identifier, e.g. acme/widgets" },
            "view_id": { "type": "string", "description": "view id, e.g. main" },
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
            "repo": { "type": "string", "description": "repository identifier, e.g. acme/widgets" },
            "view_id": { "type": "string", "description": "view id, e.g. main" },
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
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        let mut manifest = Manifest::new();
        for (path, src) in [
            ("lib.rs", "fn helper() {}"),
            ("main.rs", "fn main() { helper(); }"),
        ] {
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
                "status", "search", "node", "callers", "callees", "impact", "explore", "diff",
                "overview", "top", "list",
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

    #[tokio::test]
    async fn aggregate_tools_require_a_view_selector() {
        let s = seeded_server().await;
        for tool in ["overview", "top", "list"] {
            let result = s.dispatch(tool, None).await.unwrap();
            assert_eq!(result.is_error, Some(true), "{tool} should reject no args");
            assert!(result_text(&result).contains("repo"));
        }
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

    #[test]
    fn status_reports_ok_and_configured_root() {
        let s = server("/tmp/some-root");
        let payload = s.status_json();
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
