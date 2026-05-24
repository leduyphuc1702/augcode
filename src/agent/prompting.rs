use super::Agent;
use crate::logging;
use crate::message::{Message, ToolDefinition};

impl Agent {
    pub(super) fn log_prompt_prefix_accounting(
        &self,
        split: &crate::prompt::SplitSystemPrompt,
        tools: &[ToolDefinition],
    ) {
        let system_tokens = split.estimated_tokens();
        let tool_tokens = ToolDefinition::aggregate_prompt_token_estimate(tools);
        let prefix_tokens = system_tokens + tool_tokens;
        logging::info(&format!(
            "Prompt prefix estimate: total={} tokens (system={} tools={})",
            prefix_tokens, system_tokens, tool_tokens
        ));
    }

    pub(super) fn build_memory_prompt_nonblocking_shared(
        &self,
        messages: std::sync::Arc<[Message]>,
        _memory_event_tx: Option<crate::memory::MemoryEventSink>,
    ) -> Option<crate::memory::PendingMemory> {
        if !self.memory_enabled {
            return None;
        }

        let session_id = &self.session.id;

        let pending = if crate::message::ends_with_fresh_user_turn(&messages) {
            crate::memory::take_pending_memory(session_id)
        } else {
            None
        };

        // Use the persistent memory-agent pipeline as the single source of truth.
        // Running both this and the legacy MemoryManager background retrieval path
        // can prepare overlapping pending prompts for the same turn, which makes
        // memory injection feel overly aggressive.
        crate::memory_agent::update_context_sync_with_runtime(
            session_id,
            messages,
            self.session.working_dir.clone(),
            self.provider.name().to_string(),
            self.provider.model(),
        );

        pending
    }

    fn append_current_turn_system_reminder(&self, split: &mut crate::prompt::SplitSystemPrompt) {
        let Some(reminder) = self
            .current_turn_system_reminder
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        else {
            return;
        };

        if !split.dynamic_part.is_empty() {
            split.dynamic_part.push_str("\n\n");
        }
        split.dynamic_part.push_str("# System Reminder\n\n");
        split.dynamic_part.push_str(reminder);
    }

    /// Build split system prompt for better caching
    /// Returns static (cacheable) and dynamic (not cached) parts separately
    pub(super) fn build_system_prompt_split(
        &self,
        memory_prompt: Option<&str>,
    ) -> crate::prompt::SplitSystemPrompt {
        if let Some(ref override_prompt) = self.system_prompt_override {
            return crate::prompt::SplitSystemPrompt {
                static_part: override_prompt.clone(),
                dynamic_part: String::new(),
            };
        }

        let skills = self.current_skills_snapshot();
        let skill_prompt = self.active_skill.as_ref().and_then(|name| {
            skills.get(name).map(|skill| {
                if crate::skill_router::enabled() {
                    skill.get_untrusted_prompt()
                } else {
                    skill.get_prompt()
                }
            })
        });

        let use_router = crate::skill_router::enabled();
        let available_skills: Vec<crate::prompt::SkillInfo> = if use_router {
            Vec::new()
        } else {
            skills
                .list()
                .iter()
                .map(|skill| crate::prompt::SkillInfo {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                })
                .collect()
        };

        let working_dir = self
            .session
            .working_dir
            .as_ref()
            .map(std::path::PathBuf::from);

        let (mut split, _context_info) = crate::prompt::build_system_prompt_split(
            skill_prompt.as_deref(),
            &available_skills,
            self.session.is_canary,
            memory_prompt,
            working_dir.as_deref(),
        );

        if crate::agent_workflow::enabled() {
            let role = crate::agent_workflow::effective_role(&self.session);
            if !split.dynamic_part.is_empty() {
                split.dynamic_part.push_str("\n\n");
            }
            split
                .dynamic_part
                .push_str(&crate::agent_workflow::workflow_prompt_for_role(&role));
        }

        let route_messages = self
            .session
            .messages
            .iter()
            .map(|message| message.to_message())
            .collect::<Vec<_>>();
        if use_router
            && let Some(prompt_text) = crate::skill_router::latest_user_text(&route_messages)
        {
            let manifests = skills.manifests();
            let role = crate::agent_workflow::effective_role(&self.session);
            let agent = crate::skill_router::AgentProfile::from_allowed_tools(
                self.session.id.clone(),
                role,
                self.allowed_tools.as_ref(),
            );
            let routing = crate::skill_router::route_for_prompt(
                &manifests,
                &prompt_text,
                working_dir.as_deref(),
                &agent,
                "turn",
            );
            if let Some(context) =
                crate::skill_router::render_manifest_context(&routing, &manifests)
            {
                if !split.dynamic_part.is_empty() {
                    split.dynamic_part.push_str("\n\n");
                }
                split.dynamic_part.push_str(&context);
            }
        }

        self.append_current_turn_system_reminder(&mut split);

        split
    }

    /// Non-blocking memory prompt - takes pending result and spawns check for next turn
    pub(super) fn build_memory_prompt_nonblocking(
        &self,
        messages: &[Message],
        _memory_event_tx: Option<crate::memory::MemoryEventSink>,
    ) -> Option<crate::memory::PendingMemory> {
        self.build_memory_prompt_nonblocking_shared(messages.to_vec().into(), _memory_event_tx)
    }

    pub(super) fn build_codebase_context_prompt(&self, messages: &[Message]) -> Option<String> {
        crate::codebase_context::build_codebase_context_prompt(
            &self.session.id,
            self.session.working_dir.as_deref(),
            messages,
        )
    }
}
