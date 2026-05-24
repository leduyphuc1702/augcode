use super::{Registry, Tool, ToolContext, ToolOutput};
use crate::agent::Agent;
use crate::bus::{Bus, BusEvent, ToolSummary, ToolSummaryState};
use crate::logging;
use crate::protocol::HistoryMessage;
use crate::provider::Provider;
use crate::session::Session;
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

pub struct SubagentTool {
    provider: Arc<dyn Provider>,
    registry: Registry,
}

impl SubagentTool {
    pub fn new(provider: Arc<dyn Provider>, registry: Registry) -> Self {
        Self { provider, registry }
    }

    fn preferred_parent_subagent_model(parent_session_id: &str) -> Option<String> {
        Session::load(parent_session_id)
            .ok()
            .and_then(|session| session.subagent_model)
    }

    fn resolve_model(
        requested_model: Option<&str>,
        existing_session_model: Option<&str>,
        workflow_role_model: Option<&str>,
        parent_subagent_model: Option<&str>,
        provider_model: &str,
    ) -> String {
        requested_model
            .or(existing_session_model)
            .or(workflow_role_model)
            .or(parent_subagent_model)
            .or(crate::config::config().agents.swarm_model.as_deref())
            .unwrap_or(provider_model)
            .to_string()
    }
}

#[derive(Deserialize)]
struct SubagentInput {
    description: String,
    prompt: String,
    subagent_type: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    workflow_task_id: Option<String>,
    #[serde(default)]
    output_mode: SubagentOutputMode,
    #[serde(rename = "command", default)]
    _command: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SubagentOutputMode {
    /// Return only the subagent's final answer plus metadata. This preserves the
    /// historical low-token default for ordinary delegation.
    #[default]
    Answer,
    /// Return the final answer plus a human-readable transcript similar to what
    /// a user would inspect: roles, text, tool calls, and tool results.
    Compact,
    /// Return the final answer plus the persisted raw child session messages as
    /// pretty JSON for debugging/auditing.
    FullTranscript,
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "Run a subagent."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["description", "prompt", "subagent_type"],
            "properties": {
                "intent": super::intent_schema_property(),
                "description": {
                    "type": "string",
                    "description": "Task description."
                },
                "prompt": {
                    "type": "string",
                    "description": "Task prompt."
                },
                "subagent_type": {
                    "type": "string",
                    "description": "Subagent type."
                },
                "model": {
                    "type": "string",
                    "description": "Model override."
                },
                "session_id": {
                    "type": "string",
                    "description": "Existing session ID."
                },
                "workflow_task_id": {
                    "type": "string",
                    "description": "Stable workflow task id. When agent_workflow is enabled, role/task resumes the same child session."
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["answer", "compact", "full_transcript"],
                    "description": "Return mode. 'answer' returns the final answer only, 'compact' adds a user-visible transcript, and 'full_transcript' adds raw persisted messages. Defaults to 'answer'."
                },
                "command": {
                    "type": "string",
                    "description": "Source command."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: SubagentInput = serde_json::from_value(input)?;
        let workflow_enabled = crate::agent_workflow::enabled();
        let workflow_role = if workflow_enabled {
            if !crate::agent_workflow::is_known_role(&params.subagent_type) {
                anyhow::bail!(
                    "unknown workflow subagent role '{}'. Allowed: {}",
                    params.subagent_type,
                    crate::agent_workflow::roles().join(", ")
                );
            }
            if params.output_mode != SubagentOutputMode::Answer {
                anyhow::bail!(
                    "agent_workflow subagents return answer artifacts only; inspect child session explicitly for transcripts"
                );
            }
            Some(params.subagent_type.as_str())
        } else {
            None
        };
        let workflow_task_id = params
            .workflow_task_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if workflow_enabled && workflow_task_id.is_none() {
            anyhow::bail!("workflow_task_id is required when agent_workflow is enabled");
        }

        let parent_state = if workflow_enabled {
            Session::load(&ctx.session_id)
                .ok()
                .and_then(|session| session.agent_workflow_state)
        } else {
            None
        };
        if let Some(role) = workflow_role {
            crate::agent_workflow::role_can_spawn(parent_state.as_ref(), role)
                .map_err(anyhow::Error::msg)?;
            enforce_workflow_context_guard(&self.registry, self.provider.clone(), &ctx.session_id)?;
        }

        let resume_session_id = params.session_id.clone().or_else(|| {
            workflow_role
                .zip(workflow_task_id)
                .and_then(|(role, task_id)| {
                    crate::agent_workflow::find_existing_child_session(
                        &ctx.session_id,
                        role,
                        task_id,
                    )
                })
        });

        let mut session = if let Some(session_id) = &resume_session_id {
            Session::load(session_id).unwrap_or_else(|err| {
                logging::warn(&format!(
                    "[tool:subagent] failed to load existing session {}; creating a new subagent session instead: {}",
                    session_id, err
                ));
                Session::create(Some(ctx.session_id.clone()), Some(subagent_title(&params)))
            })
        } else {
            Session::create(Some(ctx.session_id.clone()), Some(subagent_title(&params)))
        };
        if let Some(role) = workflow_role {
            validate_workflow_child_session(&session, &ctx.session_id, role, workflow_task_id)?;
            session.parent_id = Some(ctx.session_id.clone());
            session.agent_role = Some(role.to_string());
            session.workflow_task_id = workflow_task_id.map(str::to_string);
        }
        let parent_subagent_model = Self::preferred_parent_subagent_model(&ctx.session_id);
        let workflow_role_model =
            workflow_role.and_then(crate::agent_workflow::role_model_override);
        let provider_model = self.provider.model();
        let resolved_model = Self::resolve_model(
            params.model.as_deref(),
            session.model.as_deref(),
            workflow_role_model.as_deref(),
            parent_subagent_model.as_deref(),
            &provider_model,
        );
        session.model = Some(resolved_model.clone());

        if let Some(ref working_dir) = ctx.working_dir {
            session.working_dir = Some(working_dir.display().to_string());
        }

        session.save()?;
        if let (Some(role), Some(task_id)) = (workflow_role, workflow_task_id) {
            update_parent_workflow_task(
                &ctx.session_id,
                task_id,
                role,
                &session.id,
                crate::agent_workflow::role_status_for_start(role),
                None,
            )?;
        }

        let mut allowed: HashSet<String> = self.registry.tool_names().await.into_iter().collect();
        for blocked in ["subagent", "task", "todo", "todowrite", "todoread"] {
            allowed.remove(blocked);
        }
        if let Some(role) = workflow_role {
            let all_names = allowed.iter().cloned().collect::<Vec<_>>();
            allowed = crate::agent_workflow::role_allowed_tools(role, &all_names);
        }

        let summary_map: Arc<Mutex<HashMap<String, ToolSummary>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let summary_map_handle = summary_map.clone();
        let session_id = session.id.clone();

        let mut receiver = Bus::global().subscribe();
        let listener = tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(BusEvent::ToolUpdated(event)) => {
                        if event.session_id != session_id {
                            continue;
                        }
                        let mut summary = summary_map_handle
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        summary.insert(
                            event.tool_call_id.clone(),
                            ToolSummary {
                                id: event.tool_call_id.clone(),
                                tool: event.tool_name.clone(),
                                state: ToolSummaryState {
                                    status: event.status.as_str().to_string(),
                                    title: if event.status.as_str() == "completed" {
                                        event.title.clone()
                                    } else {
                                        None
                                    },
                                },
                            },
                        );
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });

        logging::info(&format!(
            "Subagent starting: {} (type: {})",
            params.description, params.subagent_type
        ));

        // Run subagent on an isolated provider fork so model/session changes do not
        // mutate the coordinator's provider instance.
        let mut agent = Agent::new_with_session(
            self.provider.fork(),
            self.registry.clone(),
            session,
            Some(allowed),
        );

        let prompt = if let Some(role) = workflow_role {
            format!(
                "{}{}",
                params.prompt,
                crate::agent_workflow::artifact_prompt(role, workflow_task_id)
            )
        } else {
            params.prompt.clone()
        };

        let start = std::time::Instant::now();
        let final_text = agent.run_once_capture(&prompt).await.map_err(|err| {
            logging::warn(&format!(
                "[tool:subagent] subagent failed description={} type={} session_id={} model={} error={}",
                params.description,
                params.subagent_type,
                agent.session_id(),
                resolved_model,
                err
            ));
            err
        })?;
        let sub_session_id = agent.session_id().to_string();
        let history = if params.output_mode == SubagentOutputMode::Compact {
            Some(agent.get_history())
        } else {
            None
        };
        let full_transcript = if params.output_mode == SubagentOutputMode::FullTranscript {
            let session = Session::load(&sub_session_id)?;
            Some(serde_json::to_string_pretty(&session.messages)?)
        } else {
            None
        };

        logging::info(&format!(
            "Subagent completed: {} in {:.1}s",
            params.description,
            start.elapsed().as_secs_f64()
        ));

        listener.abort();

        let mut summary: Vec<ToolSummary> = summary_map
            .lock()
            .map_err(|_| anyhow::anyhow!("tool summary lock poisoned"))?
            .values()
            .cloned()
            .collect();
        summary.sort_by(|a, b| a.id.cmp(&b.id));

        let (artifact_text, artifact_truncated) = if workflow_enabled {
            crate::agent_workflow::cap_artifact(&final_text)
        } else {
            (final_text.clone(), false)
        };
        if let (Some(role), Some(task_id)) = (workflow_role, workflow_task_id) {
            let mut task = workflow_task_state(
                task_id,
                role,
                &sub_session_id,
                crate::agent_workflow::role_status_for_complete(role),
                Some(crate::agent_workflow::artifact_summary(&artifact_text)),
            );
            if role == crate::agent_workflow::ROLE_PLAN_FINALIZER {
                update_parent_final_plan(&ctx.session_id, &artifact_text)?;
            } else if role == crate::agent_workflow::ROLE_CODE_REVIEWER {
                update_parent_review(&ctx.session_id, &artifact_text)?;
            } else {
                update_parent_workflow_task_state(&ctx.session_id, task.clone())?;
            }
            task.status = crate::agent_workflow::role_status_for_complete(role).to_string();
            update_parent_workflow_task_state(&ctx.session_id, task)?;
        }

        let output = format_subagent_output(
            &artifact_text,
            &sub_session_id,
            params.output_mode,
            history.as_deref(),
            full_transcript.as_deref(),
        );

        Ok(ToolOutput::new(output)
            .with_title(subagent_display_title(&params, &resolved_model))
            .with_metadata(json!({
                "summary": summary,
                "sessionId": sub_session_id,
                "model": resolved_model,
                "outputMode": params.output_mode.as_str(),
                "artifactTruncated": artifact_truncated,
                "workflowTaskId": workflow_task_id,
                "agentRole": workflow_role,
            })))
    }
}

fn validate_workflow_child_session(
    session: &Session,
    parent_id: &str,
    role: &str,
    task_id: Option<&str>,
) -> Result<()> {
    if let Some(existing_parent) = session.parent_id.as_deref()
        && existing_parent != parent_id
    {
        anyhow::bail!(
            "workflow child session '{}' belongs to a different parent",
            session.id
        );
    }
    if let Some(existing_role) = session.agent_role.as_deref()
        && existing_role != role
    {
        anyhow::bail!(
            "workflow child session '{}' role mismatch: {} != {}",
            session.id,
            existing_role,
            role
        );
    }
    if let (Some(existing_task), Some(task_id)) = (session.workflow_task_id.as_deref(), task_id)
        && existing_task != task_id
    {
        anyhow::bail!(
            "workflow child session '{}' task mismatch: {} != {}",
            session.id,
            existing_task,
            task_id
        );
    }
    Ok(())
}

fn workflow_task_state(
    task_id: &str,
    role: &str,
    session_id: &str,
    status: &str,
    summary: Option<String>,
) -> crate::agent_workflow::AgentWorkflowTaskState {
    crate::agent_workflow::AgentWorkflowTaskState {
        id: task_id.to_string(),
        agent_role: role.to_string(),
        session_id: session_id.to_string(),
        status: status.to_string(),
        summary,
        ..Default::default()
    }
}

fn update_parent_workflow_task(
    parent_id: &str,
    task_id: &str,
    role: &str,
    session_id: &str,
    status: &str,
    summary: Option<String>,
) -> Result<()> {
    update_parent_workflow_task_state(
        parent_id,
        workflow_task_state(task_id, role, session_id, status, summary),
    )
}

fn update_parent_workflow_task_state(
    parent_id: &str,
    task: crate::agent_workflow::AgentWorkflowTaskState,
) -> Result<()> {
    let mut parent = Session::load(parent_id)?;
    let mut state = parent.agent_workflow_state.clone().unwrap_or_default();
    if task.summary.is_none() {
        state.set_phase_for_role_start(&task.agent_role);
    } else {
        state.status = task.status.clone();
    }
    state.upsert_task(task);
    parent.agent_workflow_state = Some(state);
    parent.save()
}

fn update_parent_final_plan(parent_id: &str, plan: &str) -> Result<()> {
    let mut parent = Session::load(parent_id)?;
    let mut state = parent.agent_workflow_state.clone().unwrap_or_default();
    state.submit_final_plan(plan.to_string());
    parent.agent_workflow_state = Some(state);
    parent.save()
}

fn update_parent_review(parent_id: &str, review: &str) -> Result<()> {
    let mut parent = Session::load(parent_id)?;
    let mut state = parent.agent_workflow_state.clone().unwrap_or_default();
    state.submit_review(review.to_string());
    parent.agent_workflow_state = Some(state);
    parent.save()
}

fn enforce_workflow_context_guard(
    registry: &Registry,
    provider: Arc<dyn Provider>,
    parent_id: &str,
) -> Result<()> {
    if !provider.uses_jcode_compaction() {
        return Ok(());
    }
    let mut parent = Session::load(parent_id)?;
    let messages = parent.provider_messages().to_vec();
    let compaction = registry.compaction();
    let mut manager = compaction
        .try_write()
        .map_err(|_| anyhow::anyhow!("workflow context guard could not acquire compaction lock"))?;
    let action = manager.ensure_context_fits(&messages, provider);
    let hard_pct = crate::config::config()
        .workflow
        .orchestrator_context_hard_pct;
    if manager.context_usage_with(&messages) >= hard_pct
        && !matches!(
            action,
            crate::compaction::CompactionAction::HardCompacted(_)
        )
    {
        anyhow::bail!(
            "workflow context guard stopped delegation at {:.0}% context; run /compact or reduce scope",
            hard_pct * 100.0
        );
    }
    Ok(())
}

fn subagent_title(params: &SubagentInput) -> String {
    format!(
        "{} (@{} subagent)",
        params.description, params.subagent_type
    )
}

fn subagent_display_title(params: &SubagentInput, model: &str) -> String {
    format!(
        "{} ({} · {})",
        params.description, params.subagent_type, model
    )
}

impl SubagentOutputMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Compact => "compact",
            Self::FullTranscript => "full_transcript",
        }
    }
}

fn format_subagent_output(
    final_text: &str,
    sub_session_id: &str,
    output_mode: SubagentOutputMode,
    history: Option<&[HistoryMessage]>,
    full_transcript: Option<&str>,
) -> String {
    let mut output = final_text.to_string();
    if !output.ends_with('\n') {
        output.push('\n');
    }

    match output_mode {
        SubagentOutputMode::Answer => {}
        SubagentOutputMode::Compact => {
            output.push_str("\n## Subagent transcript (compact)\n\n");
            output.push_str(&format_compact_subagent_history(history.unwrap_or(&[])));
        }
        SubagentOutputMode::FullTranscript => {
            output.push_str("\n## Subagent transcript (full)\n\n```json\n");
            output.push_str(full_transcript.unwrap_or("[]"));
            output.push_str("\n```\n");
        }
    }

    output.push('\n');
    output.push_str("<subagent_metadata>\n");
    output.push_str(&format!("session_id: {}\n", sub_session_id));
    output.push_str(&format!("output_mode: {}\n", output_mode.as_str()));
    output.push_str("</subagent_metadata>");
    output
}

fn format_compact_subagent_history(messages: &[HistoryMessage]) -> String {
    if messages.is_empty() {
        return "(empty transcript)\n".to_string();
    }

    let mut output = String::new();
    for (index, message) in messages.iter().enumerate() {
        output.push_str(&format!("### {}. {}\n\n", index + 1, message.role));
        if !message.content.trim().is_empty() {
            output.push_str(message.content.trim());
            output.push_str("\n\n");
        }
        if let Some(tool_calls) = &message.tool_calls
            && !tool_calls.is_empty()
        {
            output.push_str("Tool calls:\n");
            for call in tool_calls {
                output.push_str(&format!("- `{}`\n", call));
            }
            output.push('\n');
        }
        if let Some(tool_data) = &message.tool_data {
            output.push_str("Tool result:\n");
            output.push_str("```json\n");
            match serde_json::to_string_pretty(tool_data) {
                Ok(json) => output.push_str(&json),
                Err(_) => output.push_str("<unserializable tool data>"),
            }
            output.push_str("\n```\n\n");
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{
        SubagentInput, SubagentOutputMode, format_compact_subagent_history, format_subagent_output,
        subagent_display_title, validate_workflow_child_session, workflow_task_state,
    };
    use crate::protocol::HistoryMessage;
    use crate::session::Session;

    #[test]
    fn subagent_display_title_includes_type_and_model() {
        let params = SubagentInput {
            description: "Verify subagent model".to_string(),
            prompt: "prompt".to_string(),
            subagent_type: "general".to_string(),
            model: None,
            session_id: None,
            workflow_task_id: None,
            output_mode: SubagentOutputMode::Answer,
            _command: None,
        };

        assert_eq!(
            subagent_display_title(&params, "gpt-5.4"),
            "Verify subagent model (general · gpt-5.4)"
        );
    }

    #[test]
    fn resolve_model_prefers_explicit_then_existing_then_parent_then_provider() {
        assert_eq!(
            super::SubagentTool::resolve_model(
                Some("explicit"),
                Some("existing"),
                Some("role"),
                Some("parent"),
                "provider"
            ),
            "explicit"
        );
        assert_eq!(
            super::SubagentTool::resolve_model(
                None,
                Some("existing"),
                Some("role"),
                Some("parent"),
                "provider"
            ),
            "existing"
        );
        assert_eq!(
            super::SubagentTool::resolve_model(
                None,
                None,
                Some("role"),
                Some("parent"),
                "provider"
            ),
            "role"
        );
        assert_eq!(
            super::SubagentTool::resolve_model(None, None, None, Some("parent"), "provider"),
            "parent"
        );
        let configured_or_provider = crate::config::config()
            .agents
            .swarm_model
            .as_deref()
            .unwrap_or("provider");
        assert_eq!(
            super::SubagentTool::resolve_model(None, None, None, None, "provider"),
            configured_or_provider
        );
    }

    #[test]
    fn format_subagent_output_preserves_answer_without_generic_next_step_footer() {
        let output = format_subagent_output(
            "answer",
            "session_test",
            SubagentOutputMode::Answer,
            None,
            None,
        );

        assert!(output.starts_with("answer\n\n<subagent_metadata>\n"));
        assert!(output.contains("session_id: session_test\n"));
        assert!(output.contains("output_mode: answer\n"));
        assert!(!output.contains("Next step: integrate this result"));
    }

    #[test]
    fn compact_output_includes_human_readable_history() {
        let history = vec![HistoryMessage {
            role: "assistant".to_string(),
            content: "I will inspect it.".to_string(),
            tool_calls: Some(vec!["read".to_string()]),
            tool_data: None,
        }];
        let output = format_subagent_output(
            "final answer",
            "session_test",
            SubagentOutputMode::Compact,
            Some(&history),
            None,
        );

        assert!(output.contains("## Subagent transcript (compact)"));
        assert!(output.contains("### 1. assistant"));
        assert!(output.contains("I will inspect it."));
        assert!(output.contains("- `read`"));
        assert!(output.contains("output_mode: compact\n"));
    }

    #[test]
    fn full_transcript_output_includes_raw_json_section() {
        let output = format_subagent_output(
            "final answer",
            "session_test",
            SubagentOutputMode::FullTranscript,
            None,
            Some("[{\"role\":\"user\"}]"),
        );

        assert!(output.contains("## Subagent transcript (full)"));
        assert!(output.contains("```json\n[{\"role\":\"user\"}]\n```"));
        assert!(output.contains("output_mode: full_transcript\n"));
    }

    #[test]
    fn compact_history_formats_empty_transcript() {
        assert_eq!(format_compact_subagent_history(&[]), "(empty transcript)\n");
    }

    #[test]
    fn workflow_child_session_validation_rejects_role_or_task_mismatch() {
        let mut session = Session::create_with_id(
            "workflow_child_validation".to_string(),
            Some("parent".to_string()),
            None,
        );
        session.agent_role = Some(crate::agent_workflow::ROLE_FRONTEND.to_string());
        session.workflow_task_id = Some("task-ui".to_string());

        assert!(
            validate_workflow_child_session(
                &session,
                "parent",
                crate::agent_workflow::ROLE_FRONTEND,
                Some("task-ui")
            )
            .is_ok()
        );
        assert!(
            validate_workflow_child_session(
                &session,
                "parent",
                crate::agent_workflow::ROLE_BACKEND,
                Some("task-ui")
            )
            .is_err()
        );
        assert!(
            validate_workflow_child_session(
                &session,
                "parent",
                crate::agent_workflow::ROLE_FRONTEND,
                Some("task-api")
            )
            .is_err()
        );
    }

    #[test]
    fn workflow_task_state_records_role_session_status() {
        let task = workflow_task_state(
            "task-ui",
            crate::agent_workflow::ROLE_FRONTEND,
            "session-child",
            crate::agent_workflow::STATUS_IMPLEMENTING,
            Some("artifact summary".to_string()),
        );

        assert_eq!(task.id, "task-ui");
        assert_eq!(task.agent_role, crate::agent_workflow::ROLE_FRONTEND);
        assert_eq!(task.session_id, "session-child");
        assert_eq!(task.status, crate::agent_workflow::STATUS_IMPLEMENTING);
        assert_eq!(task.summary.as_deref(), Some("artifact summary"));
    }
}
