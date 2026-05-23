use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use jcode_codebase_retrieval::root_from_context_path;
use jcode_codebase_sync::{LocalCasSyncClient, SyncOutbox};
use serde_json::{Value, json};

pub struct SyncDrainTool;

impl SyncDrainTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for SyncDrainTool {
    fn name(&self) -> &str {
        "sync_drain"
    }

    fn description(&self) -> &str {
        "Drain ready local codebase sync outbox jobs into the local CAS backend."
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
        let state_dir = jcode_storage::jcode_dir()?
            .join("codebase")
            .join(jcode_codebase_sync::workspace_id(&root));
        let outbox = SyncOutbox::new(state_dir.join("outbox.jsonl"));
        let client = LocalCasSyncClient::new(state_dir.join("cas"));
        let drained = outbox.drain_ready(&root, &client)?;
        Ok(ToolOutput::new(format!("drained_jobs: {}\n", drained)).with_title("sync_drain"))
    }
}
