use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub struct AgentWorkflowTool;

impl AgentWorkflowTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct AgentWorkflowInput {
    action: String,
    #[serde(default)]
    content: Option<String>,
}

#[async_trait]
impl Tool for AgentWorkflowTool {
    fn name(&self) -> &str {
        "agent_workflow"
    }

    fn description(&self) -> &str {
        "Manage the opt-in session-per-agent workflow."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["status", "submit_final_plan", "submit_review", "reset"],
                    "description": "Workflow action."
                },
                "content": {
                    "type": "string",
                    "description": "Final plan or review content for submit_* actions."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: AgentWorkflowInput = serde_json::from_value(input)?;
        let mut session = crate::session::Session::load(&ctx.session_id)?;
        let mut state = session.agent_workflow_state.clone().unwrap_or_default();

        match params.action.as_str() {
            "status" => Ok(
                ToolOutput::new(crate::agent_workflow::render_status(&session))
                    .with_title("agent_workflow: status"),
            ),
            "submit_final_plan" => {
                ensure_enabled()?;
                let content = required_content(params.content, "submit_final_plan")?;
                state.submit_final_plan(content);
                session.agent_workflow_state = Some(state);
                session.save()?;
                Ok(
                    ToolOutput::new("Final plan submitted. Stop and wait for `/approve-plan`.")
                        .with_title("agent_workflow: awaiting plan approval"),
                )
            }
            "submit_review" => {
                ensure_enabled()?;
                let content = required_content(params.content, "submit_review")?;
                state.submit_review(content);
                session.agent_workflow_state = Some(state);
                session.save()?;
                Ok(
                    ToolOutput::new("Review submitted. Stop and wait for `/approve-review`.")
                        .with_title("agent_workflow: awaiting review approval"),
                )
            }
            "reset" => {
                ensure_enabled()?;
                session.agent_workflow_state = Some(Default::default());
                session.save()?;
                Ok(ToolOutput::new("Workflow state reset.").with_title("agent_workflow: reset"))
            }
            other => anyhow::bail!("unknown agent_workflow action '{}'", other),
        }
    }
}

fn ensure_enabled() -> Result<()> {
    if crate::agent_workflow::enabled() {
        Ok(())
    } else {
        anyhow::bail!("agent_workflow is disabled")
    }
}

fn required_content(content: Option<String>, action: &str) -> Result<String> {
    let content = content.unwrap_or_default();
    let trimmed = content.trim();
    if trimmed.is_empty() {
        anyhow::bail!("content is required for {}", action);
    }
    Ok(trimmed.to_string())
}
