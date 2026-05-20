use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use jcode_codebase_retrieval::root_from_context_path;
use jcode_codebase_sync::{CodebaseSyncEngine, ManifestStore, SnapshotTokenPayload};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct ReadFileTool;

impl ReadFileTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct ReadFileInput {
    path: String,
    #[serde(default)]
    snapshot_token: Option<String>,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a file from the current workspace snapshot with authorization. \
         Rejects paths not present in the current snapshot or with mismatched content hash."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "intent": super::intent_schema_property(),
                "path": {
                    "type": "string",
                    "description": "Relative path within the workspace."
                },
                "snapshot_token": {
                    "type": "string",
                    "description": "Optional snapshot token. If omitted, the current workspace snapshot is used."
                },
                "start_line": {
                    "type": "integer",
                    "description": "1-based start line."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max lines to read. Default 5000."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: ReadFileInput = serde_json::from_value(input)?;
        let root = root_from_context_path(ctx.working_dir)?;
        let engine = CodebaseSyncEngine::new(ManifestStore::default_store()?);

        let token = match params.snapshot_token {
            Some(token_json) => serde_json::from_str::<SnapshotTokenPayload>(&token_json)
                .map_err(|e| anyhow::anyhow!("invalid snapshot token: {}", e))?,
            None => match engine.snapshot_token(&root)? {
                Some(token) => token,
                None => {
                    engine.open_workspace(&root)?;
                    engine
                        .snapshot_token(&root)?
                        .ok_or_else(|| anyhow::anyhow!("no snapshot available for workspace"))?
                }
            },
        };

        let read = engine.read_file_authorized(&root, &params.path, &token)?;

        let content = if let Some(start) = params.start_line {
            let limit = params.limit.unwrap_or(5000);
            let lines: Vec<&str> = read.contents.lines().collect();
            let start_idx = start.saturating_sub(1);
            let end_idx = (start_idx + limit).min(lines.len());
            lines[start_idx..end_idx].join("\n")
        } else {
            read.contents
        };

        Ok(ToolOutput::new(format!(
            "{} [{}]\n{}",
            read.path, read.content_hash, content
        )))
    }
}


#[cfg(test)]
#[path = "read_file_tests.rs"]
mod tests;
