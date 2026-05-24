use super::ReadFileTool;
use crate::tool::{Tool, ToolContext, ToolExecutionMode};
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
async fn read_file_returns_authorized_content() {
    let dir = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write(
        &dir.path().join("src/lib.rs"),
        "pub fn authorized_fn() {}\n",
    );
    crate::env::set_var("JCODE_DIR", store.path());
    let tool = ReadFileTool::new();
    let out = tool
        .execute(json!({"path": "src/lib.rs"}), test_ctx(dir.path()))
        .await
        .unwrap();
    assert!(out.output.contains("authorized_fn"));
}

#[tokio::test]
async fn read_file_rejects_unauthorized_tampered_token() {
    let dir = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write(&dir.path().join("src/lib.rs"), "pub fn secret() {}\n");
    crate::env::set_var("JCODE_DIR", store.path());
    let tool = ReadFileTool::new();
    let bad_token = json!({
        "workspace_id": "w",
        "branch": null,
        "head_sha": null,
        "allowed_content_hashes": ["sha256:bad"],
        "path_to_hash": {"src/lib.rs": "sha256:bad"},
        "issued_at": "2024-01-01T00:00:00Z"
    });
    let result = tool
        .execute(
            json!({
                "path": "src/lib.rs",
                "snapshot_token": bad_token.to_string()
            }),
            test_ctx(dir.path()),
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn read_file_rejects_path_traversal() {
    let dir = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write(&dir.path().join("src/lib.rs"), "pub fn safe() {}\n");
    crate::env::set_var("JCODE_DIR", store.path());
    let tool = ReadFileTool::new();
    let result = tool
        .execute(json!({"path": "../etc/passwd"}), test_ctx(dir.path()))
        .await;
    assert!(result.is_err());
}
