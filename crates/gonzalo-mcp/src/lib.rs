//! MCP server exposing the gonzalo code graph to agents (EPIC D).
//!
//! [`GonzaloMcp`] implements rmcp's [`ServerHandler`] over a
//! [`Service`](gonzalo_server::Service): agents spawn the `gonzalo-mcp` binary
//! (stdio) and call tools that answer from the local store. This scaffold ships
//! one view-independent tool, `status`; the graph query tools land in D1b.
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

    /// The tools this server advertises.
    pub fn tools() -> Vec<Tool> {
        vec![Tool::new(
            "status",
            "Report that the gonzalo-mcp server is up and its configured store root.",
            object_schema(),
        )]
    }

    /// The `status` payload: server health plus the configured store root.
    pub fn status_json(&self) -> serde_json::Value {
        serde_json::json!({ "status": "ok", "root": self.root })
    }

    /// Dispatch a tool call by name, independent of the rmcp transport so it can
    /// be unit-tested. Unknown tools return a `method_not_found` error.
    pub async fn dispatch(
        &self,
        name: &str,
        _arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match name {
            "status" => Ok(CallToolResult::success(vec![Content::text(
                self.status_json().to_string(),
            )])),
            _ => Err(rmcp::ErrorData::method_not_found::<CallToolRequestMethod>()),
        }
    }
}

/// A permissive object input schema (tool-specific schemas arrive with D1b).
fn object_schema() -> Arc<serde_json::Map<String, serde_json::Value>> {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "type".to_string(),
        serde_json::Value::String("object".to_string()),
    );
    obj.insert(
        "additionalProperties".to_string(),
        serde_json::Value::Bool(true),
    );
    Arc::new(obj)
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
    use gonzalo_store_fs::FsStore;

    fn server(root: &str) -> GonzaloMcp {
        let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
        GonzaloMcp::new(Service::new(fs.clone(), fs), root)
    }

    #[test]
    fn advertises_the_status_tool() {
        let names: Vec<String> = GonzaloMcp::tools()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(names, vec!["status".to_string()]);
    }

    #[test]
    fn status_reports_ok_and_configured_root() {
        let s = server("/tmp/some-root");
        let payload = s.status_json();
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["root"], "/tmp/some-root");
    }

    #[tokio::test]
    async fn dispatch_status_succeeds() {
        let s = server("/r");
        let result = s.dispatch("status", None).await.unwrap();
        assert_ne!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_is_method_not_found() {
        let s = server("/r");
        assert!(s.dispatch("no_such_tool", None).await.is_err());
    }
}
