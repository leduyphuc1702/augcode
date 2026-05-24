use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use jcode_codebase_retrieval::{
    ChangeAnalysisRequest, ChangeAnalysisResponse, CodebaseRetrievalEngine, root_from_context_path,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct CodebaseChangesTool;

impl CodebaseChangesTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct CodebaseChangesInput {
    #[serde(default = "default_true")]
    include_untracked: bool,
    #[serde(default)]
    include_tests: bool,
}

fn default_true() -> bool {
    true
}

#[async_trait]
impl Tool for CodebaseChangesTool {
    fn name(&self) -> &str {
        "codebase_changes"
    }

    fn description(&self) -> &str {
        "Map current git changes to changed symbols, impact, related tests, and targeted checks."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "include_untracked": { "type": "boolean", "default": true },
                "include_tests": { "type": "boolean", "default": false }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: CodebaseChangesInput = serde_json::from_value(input)?;
        let root = root_from_context_path(ctx.working_dir)?;
        let engine = CodebaseRetrievalEngine::default_engine()?;
        let response = engine.analyze_changes(
            &root,
            ChangeAnalysisRequest {
                include_untracked: params.include_untracked,
                include_tests: params.include_tests,
            },
        )?;
        Ok(ToolOutput::new(render_changes(&response)).with_title("codebase_changes"))
    }
}

fn render_changes(response: &ChangeAnalysisResponse) -> String {
    let mut out = String::new();
    out.push_str("changed_files:\n");
    for file in &response.changed_files {
        out.push_str(&format!("- {} [{}]\n", file.path, file.status));
    }
    out.push_str("changed_symbols:\n");
    for symbol in &response.changed_symbols {
        out.push_str(&format!(
            "- {}:{}-{} {} {}\n",
            symbol.path, symbol.start_line, symbol.end_line, symbol.kind, symbol.name
        ));
    }
    out.push_str("impact:\n");
    for impact in &response.impacted {
        out.push_str(&format!(
            "- {} risk={} affected={}\n",
            impact.path, impact.risk_level, impact.affected_count
        ));
    }
    if !response.suggested_tests.is_empty() {
        out.push_str("suggested_tests:\n");
        for test in &response.suggested_tests {
            out.push_str(&format!("- {test}\n"));
        }
    }
    if !response.suggested_checks.is_empty() {
        out.push_str("suggested_checks:\n");
        for check in &response.suggested_checks {
            out.push_str(&format!("- {check}\n"));
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

    fn run_git_cmd(root: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {:?} failed", args);
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
    async fn codebase_changes_tool_reports_changed_symbols() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        run_git_cmd(dir.path(), &["init"]);
        run_git_cmd(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git_cmd(dir.path(), &["config", "user.name", "Test"]);
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn login() {\n    old_login();\n}\n",
        );
        run_git_cmd(dir.path(), &["add", "."]);
        run_git_cmd(dir.path(), &["commit", "-m", "base"]);
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn login() {\n    new_login();\n}\n",
        );
        crate::env::set_var("JCODE_DIR", store.path());
        let out = CodebaseChangesTool::new()
            .execute(json!({}), test_ctx(dir.path()))
            .await
            .unwrap();
        assert!(out.output.contains("src/auth.rs"));
        assert!(out.output.contains("login"));
    }
}
