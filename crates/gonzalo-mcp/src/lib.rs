//! MCP server exposing the gonzalo code graph to agents (EPIC D).
//!
//! [`GonzaloMcp`] implements rmcp's [`ServerHandler`] over a
//! [`Service`](gonzalo_server::Service): agents spawn the `gonzalo-mcp` binary
//! (stdio) and call tools that answer from the local store. It exposes a
//! view-independent `status` tool plus the code-graph queries
//! (`search`/`node`/`callers`/`callees`/`impact`/`explore`), each taking a
//! `(repo, view_id)` view selector.
//!
//! The tool logic lives in plain methods ([`GonzaloMcp::tools`],
//! [`GonzaloMcp::dispatch`]) so it is unit-testable without an rmcp
//! [`RequestContext`]; the trait methods are thin adapters over them.

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

    /// The tools this server advertises: a view-independent `status` plus the
    /// code-graph queries, each taking a `(repo, view_id)` view selector.
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

/// A minimal object schema for the argument-free `status` tool.
fn status_schema() -> Arc<Map<String, Value>> {
    let schema = serde_json::json!({ "type": "object", "additionalProperties": false });
    Arc::new(schema.as_object().expect("object schema").clone())
}

/// Extract the required `(repo, view_id, name)` view selector from tool args.
fn view_args(arguments: &Option<Map<String, Value>>) -> Result<(String, String, String), String> {
    let get = |key: &str| {
        arguments
            .as_ref()
            .and_then(|m| m.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("missing required string argument '{key}'"))
    };
    Ok((get("repo")?, get("view_id")?, get("name")?))
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
                "status", "search", "node", "callers", "callees", "impact", "explore",
            ]
        );
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
