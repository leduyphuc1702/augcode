use super::CommitLineageSearchTool;
use crate::tool::{Tool, ToolContext, ToolExecutionMode};
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
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    }
}

#[tokio::test]
async fn commit_lineage_search_finds_matching_commits() {
    let dir = TempDir::new().unwrap();
    run_git_cmd(dir.path(), &["init"]);
    run_git_cmd(dir.path(), &["config", "user.email", "test@example.com"]);
    run_git_cmd(dir.path(), &["config", "user.name", "Test"]);
    write(&dir.path().join("src/search.rs"), "fn search_index() {}\n");
    run_git_cmd(dir.path(), &["add", "."]);
    run_git_cmd(dir.path(), &["commit", "-m", "add search index"]);

    let tool = CommitLineageSearchTool::new();
    let out = tool
        .execute(json!({"query": "search index"}), test_ctx(dir.path()))
        .await
        .unwrap();
    assert!(out.output.contains("add search index"));
    assert!(out.output.contains("src/search.rs"));
}

#[tokio::test]
async fn commit_lineage_search_returns_empty_when_no_match() {
    let dir = TempDir::new().unwrap();
    run_git_cmd(dir.path(), &["init"]);
    run_git_cmd(dir.path(), &["config", "user.email", "test@example.com"]);
    run_git_cmd(dir.path(), &["config", "user.name", "Test"]);
    write(&dir.path().join("src/lib.rs"), "fn lib() {}\n");
    run_git_cmd(dir.path(), &["add", "."]);
    run_git_cmd(dir.path(), &["commit", "-m", "initial"]);

    let tool = CommitLineageSearchTool::new();
    let out = tool
        .execute(json!({"query": "nonexistent xyz"}), test_ctx(dir.path()))
        .await
        .unwrap();
    assert!(out.output.contains("No matching commits found"));
}
