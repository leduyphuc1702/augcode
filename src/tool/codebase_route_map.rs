use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use jcode_codebase_retrieval::{
    CodebaseRetrievalEngine, RouteMapRequest, RouteMapResponse, root_from_context_path,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct CodebaseRouteMapTool;

impl CodebaseRouteMapTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct CodebaseRouteMapInput {
    #[serde(default)]
    route: Option<String>,
}

#[async_trait]
impl Tool for CodebaseRouteMapTool {
    fn name(&self) -> &str {
        "codebase_route_map"
    }

    fn description(&self) -> &str {
        "Map route/tool consumers to handlers and downstream code using the local code graph."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "route": {
                    "type": "string",
                    "description": "Optional route or tool filter, e.g. /api/users or codebase_search."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: CodebaseRouteMapInput = serde_json::from_value(input)?;
        let root = root_from_context_path(ctx.working_dir)?;
        let engine = CodebaseRetrievalEngine::default_engine()?;
        let response = engine.route_map(
            &root,
            RouteMapRequest {
                route: params.route,
            },
        )?;
        Ok(ToolOutput::new(render_route_map(&response)).with_title("codebase_route_map"))
    }
}

fn render_route_map(response: &RouteMapResponse) -> String {
    let mut out = String::new();
    for route in &response.routes {
        out.push_str(&format!("{} [{}]\n", route.route, route.kind));
        if !route.consumers.is_empty() {
            out.push_str("  consumers:\n");
            for endpoint in &route.consumers {
                out.push_str(&format!("  - {} {}\n", endpoint.path, endpoint.reason));
            }
        }
        if !route.handlers.is_empty() {
            out.push_str("  handlers:\n");
            for endpoint in &route.handlers {
                out.push_str(&format!("  - {} {}\n", endpoint.path, endpoint.reason));
            }
        }
        if !route.downstream.is_empty() {
            out.push_str("  downstream:\n");
            for endpoint in &route.downstream {
                out.push_str(&format!("  - {} {}\n", endpoint.path, endpoint.reason));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolContext, ToolExecutionMode};
    use serde_json::json;
    use tempfile::TempDir;

    fn write(path: &std::path::Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    fn test_ctx(working_dir: &std::path::Path) -> ToolContext {
        ToolContext {
            session_id: "test".to_string(),
            message_id: "msg-1".to_string(),
            tool_call_id: "call-1".to_string(),
            working_dir: Some(working_dir.to_path_buf()),
            allowed_tools: None,
            agent_role: None,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        }
    }

    #[tokio::test]
    async fn codebase_route_map_tool_links_fetch_to_handler() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(
            &dir.path().join("src/client.ts"),
            "export function load() { return fetch('/api/users'); }\n",
        );
        write(
            &dir.path().join("src/pages/api/users.ts"),
            "export function GET() {}\n",
        );
        crate::env::set_var("JCODE_DIR", store.path());
        let out = CodebaseRouteMapTool::new()
            .execute(json!({"route": "/api/users"}), test_ctx(dir.path()))
            .await
            .unwrap();
        assert!(out.output.contains("route:/api/users"));
        assert!(out.output.contains("src/client.ts"));
        assert!(out.output.contains("src/pages/api/users.ts"));
    }
}
