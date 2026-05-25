use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use jcode_codebase_retrieval::{
    CodebaseRetrievalEngine, ImpactDirection, ImpactRequest, ImpactResponse, root_from_context_path,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct CodebaseImpactTool;

impl CodebaseImpactTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct CodebaseImpactInput {
    #[serde(default)]
    target_path: Option<String>,
    #[serde(default)]
    symbol_name: Option<String>,
    #[serde(default)]
    direction: ImpactDirection,
    #[serde(default)]
    include_tests: bool,
    #[serde(default)]
    max_depth: Option<usize>,
}

#[async_trait]
impl Tool for CodebaseImpactTool {
    fn name(&self) -> &str {
        "codebase_impact"
    }

    fn description(&self) -> &str {
        "Analyze upstream/downstream code impact for a file or symbol using the local code graph."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "target_path": { "type": "string" },
                "symbol_name": { "type": "string" },
                "direction": {
                    "type": "string",
                    "enum": ["upstream", "downstream", "both"],
                    "default": "both"
                },
                "include_tests": { "type": "boolean", "default": false },
                "max_depth": { "type": "integer", "default": 1 }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: CodebaseImpactInput = serde_json::from_value(input)?;
        let root = root_from_context_path(ctx.working_dir)?;
        let engine = CodebaseRetrievalEngine::default_engine()?;
        let response = engine.impact(
            &root,
            ImpactRequest {
                target_path: params.target_path,
                symbol_name: params.symbol_name,
                direction: params.direction,
                include_tests: params.include_tests,
                max_depth: params.max_depth,
            },
        )?;
        Ok(ToolOutput::new(render_impact(&response)).with_title("codebase_impact"))
    }
}

fn render_impact(response: &ImpactResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("risk_level: {}\n", response.risk_level));
    if let Some(path) = &response.target_path {
        out.push_str(&format!("target_path: {path}\n"));
    }
    if let Some(symbol) = &response.symbol_name {
        out.push_str(&format!("symbol_name: {symbol}\n"));
    }
    out.push_str("affected:\n");
    for item in &response.affected {
        out.push_str(&format!(
            "- {} kind={} depth={} via={} symbol={}\n",
            item.path,
            item.kind,
            item.depth,
            item.via,
            item.symbol.as_deref().unwrap_or("")
        ));
    }
    if !response.related_tests.is_empty() {
        out.push_str("related_tests:\n");
        for path in &response.related_tests {
            out.push_str(&format!("- {path}\n"));
        }
    }
    if !response.dependency_paths.is_empty() {
        out.push_str("dependency_paths:\n");
        for path in response.dependency_paths.iter().take(20) {
            out.push_str(&format!("- {}\n", path.nodes.join(" -> ")));
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
    async fn codebase_impact_tool_reports_callers() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn login() {}\n");
        write(
            &dir.path().join("src/service.rs"),
            "pub fn run() { login(); }\n",
        );
        crate::env::set_var("JCODE_DIR", store.path());
        let out = CodebaseImpactTool::new()
            .execute(
                json!({
                    "target_path": "src/auth.rs",
                    "symbol_name": "login",
                    "direction": "upstream"
                }),
                test_ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.output.contains("src/service.rs"));
    }
}
