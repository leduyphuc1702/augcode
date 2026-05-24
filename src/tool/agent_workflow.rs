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
    #[serde(default)]
    selection_mode: Option<crate::agent_workflow::WorkflowSelectionMode>,
    #[serde(default)]
    options: Vec<crate::agent_workflow::WorkflowInteractionOption>,
    #[serde(default)]
    allow_custom: Option<bool>,
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
                    "enum": ["status", "submit_final_plan", "submit_review", "request_user_input", "submit_user_answer", "reset"],
                    "description": "Workflow action."
                },
                "content": {
                    "type": "string",
                    "description": "Final plan, review, question prompt, or user answer content."
                },
                "selection_mode": {
                    "type": "string",
                    "enum": ["single", "multiple"],
                    "description": "Question selection mode for request_user_input."
                },
                "options": {
                    "type": "array",
                    "description": "Question options for request_user_input.",
                    "items": {
                        "type": "object",
                        "required": ["id", "label"],
                        "properties": {
                            "id": { "type": "string" },
                            "label": { "type": "string" },
                            "description": { "type": "string" }
                        }
                    }
                },
                "allow_custom": {
                    "type": "boolean",
                    "description": "Whether the user may answer with custom free text."
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
                    ToolOutput::new("Final plan submitted. Stop and wait for user plan approval.")
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
                    ToolOutput::new("Review submitted. Stop and wait for user review approval.")
                        .with_title("agent_workflow: awaiting review approval"),
                )
            }
            "request_user_input" => {
                ensure_enabled()?;
                let content = required_content(params.content, "request_user_input")?;
                state.request_user_input(
                    content,
                    params.selection_mode.unwrap_or_default(),
                    params.options,
                    params.allow_custom.unwrap_or(true),
                );
                session.agent_workflow_state = Some(state);
                session.save()?;
                Ok(
                    ToolOutput::new("Question submitted. Stop and wait for user response.")
                        .with_title("agent_workflow: awaiting user input"),
                )
            }
            "submit_user_answer" => {
                ensure_enabled()?;
                let _content = required_content(params.content, "submit_user_answer")?;
                state.clear_pending_user_interaction();
                session.agent_workflow_state = Some(state);
                session.save()?;
                Ok(ToolOutput::new("User answer recorded.").with_title("agent_workflow: answer"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolExecutionMode;

    struct EnvVarGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
            let prev = std::env::var_os(key);
            crate::env::set_var(key, value);
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(prev) = self.prev.take() {
                crate::env::set_var(self.key, prev);
            } else {
                crate::env::remove_var(self.key);
            }
            crate::config::invalidate_config_cache();
        }
    }

    fn test_ctx(session_id: &str, working_dir: &std::path::Path) -> ToolContext {
        ToolContext {
            session_id: session_id.to_string(),
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
    async fn request_user_input_tool_stores_pending_question() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("JCODE_HOME", temp.path().as_os_str());
        let _enabled =
            EnvVarGuard::set("JCODE_AGENT_WORKFLOW_ENABLED", std::ffi::OsStr::new("true"));
        crate::config::invalidate_config_cache();

        let mut session =
            crate::session::Session::create_with_id("workflow_tool".into(), None, None);
        session.agent_workflow_state = Some(crate::agent_workflow::AgentWorkflowState::default());
        session.save().unwrap();

        let tool = AgentWorkflowTool::new();
        tool.execute(
            json!({
                "action": "request_user_input",
                "content": "Chọn phạm vi",
                "selection_mode": "multiple",
                "allow_custom": true,
                "options": [
                    { "id": "ui", "label": "UI", "description": "Frontend" },
                    { "id": "api", "label": "API" }
                ]
            }),
            test_ctx(&session.id, temp.path()),
        )
        .await
        .expect("request_user_input should succeed");

        let saved = crate::session::Session::load(&session.id).unwrap();
        let pending = saved
            .agent_workflow_state
            .unwrap()
            .pending_user_interaction
            .expect("pending interaction");
        assert_eq!(
            pending.kind,
            crate::agent_workflow::WorkflowInteractionKind::Question
        );
        assert_eq!(
            pending.selection_mode,
            crate::agent_workflow::WorkflowSelectionMode::Multiple
        );
        assert_eq!(pending.options.len(), 2);
        assert!(pending.allow_custom);
    }
}
