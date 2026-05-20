use super::CodebaseSearchTool;
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
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    }
}

#[tokio::test]
async fn e2e_edit_file_then_search_finds_new_content() {
    let dir = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write(&dir.path().join("src/auth.rs"), "pub fn old_login() {}\n");
    crate::env::set_var("JCODE_DIR", store.path());

    let tool = CodebaseSearchTool::new();

    // First search finds old content
    let out1 = tool
        .execute(
            json!({"query": "new validation logic"}),
            test_ctx(dir.path()),
        )
        .await
        .unwrap();
    assert!(!out1.output.contains("validate_password"));

    // Edit file
    write(
        &dir.path().join("src/auth.rs"),
        "pub fn new_login() { validate_password(); }\n",
    );

    // Search finds new content
    let out2 = tool
        .execute(
            json!({"query": "new validation logic"}),
            test_ctx(dir.path()),
        )
        .await
        .unwrap();
    assert!(out2.output.contains("validate_password"));
}

#[tokio::test]
async fn e2e_branch_switch_retrieval_reflects_branch() {
    let dir = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    run_git_cmd(dir.path(), &["init"]);
    run_git_cmd(dir.path(), &["config", "user.email", "test@example.com"]);
    run_git_cmd(dir.path(), &["config", "user.name", "Test"]);
    write(
        &dir.path().join("src/lib.rs"),
        "pub fn branch_symbol() { main_only(); }\n",
    );
    run_git_cmd(dir.path(), &["add", "."]);
    run_git_cmd(dir.path(), &["commit", "-m", "main"]);
    crate::env::set_var("JCODE_DIR", store.path());

    let tool = CodebaseSearchTool::new();

    // Search on main
    let out_main = tool
        .execute(json!({"query": "main only"}), test_ctx(dir.path()))
        .await
        .unwrap();
    assert!(out_main.output.contains("main_only"));

    // Switch branch
    run_git_cmd(dir.path(), &["checkout", "-b", "feature"]);
    write(
        &dir.path().join("src/lib.rs"),
        "pub fn branch_symbol() { feature_only(); }\n",
    );
    run_git_cmd(dir.path(), &["add", "."]);
    run_git_cmd(dir.path(), &["commit", "-m", "feature"]);

    // Search on feature
    let out_feature = tool
        .execute(json!({"query": "feature only"}), test_ctx(dir.path()))
        .await
        .unwrap();
    assert!(out_feature.output.contains("feature_only"));
    assert!(!out_feature.output.contains("main_only"));
}

#[tokio::test]
async fn e2e_deleted_file_not_returned() {
    let dir = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write(&dir.path().join("src/old.rs"), "pub fn old_fn() {}\n");
    crate::env::set_var("JCODE_DIR", store.path());

    let tool = CodebaseSearchTool::new();

    // Search finds file
    let out1 = tool
        .execute(json!({"query": "old fn"}), test_ctx(dir.path()))
        .await
        .unwrap();
    assert!(out1.output.contains("old.rs"));

    // Delete file
    std::fs::remove_file(dir.path().join("src/old.rs")).unwrap();

    // Search no longer finds file
    let out2 = tool
        .execute(json!({"query": "old fn"}), test_ctx(dir.path()))
        .await
        .unwrap();
    assert!(!out2.output.contains("old.rs"));
}
