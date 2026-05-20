use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use jcode_codebase_retrieval::root_from_context_path;
use jcode_codebase_sync::{CodebaseSyncEngine, ManifestStore};
use serde_json::{Value, json};

pub struct SyncStatusTool;

impl SyncStatusTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for SyncStatusTool {
    fn name(&self) -> &str {
        "sync_status"
    }

    fn description(&self) -> &str {
        "Show local codebase sync status for the current workspace."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property()
            }
        })
    }

    async fn execute(&self, _input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let root = root_from_context_path(ctx.working_dir)?;
        let engine = CodebaseSyncEngine::new(ManifestStore::default_store()?);
        let status = match engine.status(&root)? {
            Some(status) => status,
            None => status_from_manifest(engine.open_workspace(&root)?.0),
        };
        let mut output = render_status(&status);
        output.push_str(&render_sync_summary(&jcode_codebase_sync::SyncStatusSummary {
            pending_uploads: 0,
            blobs_total: 0,
            committed_deltas: 0,
            queue_hot: 0,
            queue_warm: 0,
            queue_bulk: 0,
            queue_shadow: 0,
        }));
        Ok(ToolOutput::new(output).with_title("sync_status"))
    }
}

fn render_sync_summary(summary: &jcode_codebase_sync::SyncStatusSummary) -> String {
    let mut out = String::new();
    out.push_str(&format!("pending_uploads: {}\n", summary.pending_uploads));
    out.push_str(&format!("blobs_total: {}\n", summary.blobs_total));
    out.push_str(&format!("committed_deltas: {}\n", summary.committed_deltas));
    out.push_str(&format!("queue_hot: {}\n", summary.queue_hot));
    out.push_str(&format!("queue_warm: {}\n", summary.queue_warm));
    out.push_str(&format!("queue_bulk: {}\n", summary.queue_bulk));
    out.push_str(&format!("queue_shadow: {}\n", summary.queue_shadow));
    out
}

fn render_status(status: &jcode_codebase_sync::WorkspaceStatus) -> String {
    let mut out = String::new();
    out.push_str(&format!("workspace_id: {}\n", status.workspace_id));
    out.push_str(&format!("branch: {}\n", status.branch.as_deref().unwrap_or("<none>")));
    out.push_str(&format!("head_sha: {}\n", status.head_sha.as_deref().unwrap_or("<none>")));
    out.push_str(&format!("worktree_root: {}\n", status.worktree_root.as_deref().unwrap_or("<none>")));
    out.push_str(&format!("files_total: {}\n", status.files_total));
    out.push_str(&format!("last_manifest_at: {}\n", status.last_manifest_at));
    if !status.warnings.is_empty() {
        out.push_str("warnings:\n");
        for warning in &status.warnings {
            out.push_str(&format!("- {}\n", warning));
        }
    }
    out
}

fn status_from_manifest(manifest: jcode_codebase_sync::Manifest) -> jcode_codebase_sync::WorkspaceStatus {
    jcode_codebase_sync::WorkspaceStatus {
        workspace_id: manifest.workspace_id,
        branch: manifest.branch,
        head_sha: manifest.head_sha,
        worktree_root: manifest.worktree_root,
        files_total: manifest.files.len(),
        last_manifest_at: manifest.created_at,
        file_statuses: manifest
            .files
            .values()
            .map(|entry| jcode_codebase_sync::FileSyncStatus {
                path: entry.path.clone(),
                status: jcode_codebase_sync::FileSyncState::IndexedLocalOnly,
                content_hash: Some(entry.content_hash.clone()),
                last_indexed_at: Some(manifest.created_at),
                error: None,
            })
            .collect(),
        warnings: Vec::new(),
    }
}
