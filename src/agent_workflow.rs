use crate::session::Session;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

pub const ROLE_ORCHESTRATOR: &str = "agent-orchestrator";
pub const ROLE_PLAN_AGENT: &str = "plan-agent";
pub const ROLE_PLAN_REVIEWER: &str = "plan-reviewer";
pub const ROLE_PLAN_FINALIZER: &str = "plan-finalizer";
pub const ROLE_FRONTEND: &str = "frontend-agent";
pub const ROLE_BACKEND: &str = "backend-agent";
pub const ROLE_CODE_REVIEWER: &str = "code-reviewer";
pub const ROLE_IMPLEMENTER: &str = "implementer";

pub const STATUS_IDLE: &str = "idle";
pub const STATUS_CLARIFYING: &str = "clarifying";
pub const STATUS_PLANNING: &str = "planning";
pub const STATUS_AWAITING_PLAN_APPROVAL: &str = "awaiting_plan_approval";
pub const STATUS_IMPLEMENTATION_ALLOWED: &str = "implementation_allowed";
pub const STATUS_IMPLEMENTING: &str = "implementing";
pub const STATUS_REVIEWING: &str = "reviewing";
pub const STATUS_AWAITING_REVIEW_APPROVAL: &str = "awaiting_review_approval";
pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_REJECTED: &str = "rejected";

const ARTIFACT_MAX_CHARS: usize = 16_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentWorkflowTaskState {
    pub id: String,
    pub agent_role: String,
    pub session_id: String,
    pub status: String,
    pub summary: Option<String>,
    pub commands_run: Vec<String>,
    pub validation: Option<String>,
    pub risks: Vec<String>,
    pub next_action: Option<String>,
    pub handoff: Option<String>,
    pub files_changed: Vec<String>,
}

impl Default for AgentWorkflowTaskState {
    fn default() -> Self {
        Self {
            id: String::new(),
            agent_role: String::new(),
            session_id: String::new(),
            status: STATUS_IDLE.to_string(),
            summary: None,
            commands_run: Vec::new(),
            validation: None,
            risks: Vec::new(),
            next_action: None,
            handoff: None,
            files_changed: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RemoteSkillGrant {
    pub skill_ref: String,
    pub target_agent_role: Option<String>,
    pub workflow_task_id: Option<String>,
}

impl Default for RemoteSkillGrant {
    fn default() -> Self {
        Self {
            skill_ref: String::new(),
            target_agent_role: None,
            workflow_task_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentWorkflowState {
    pub status: String,
    pub final_plan: Option<String>,
    pub review: Option<String>,
    pub tasks: Vec<AgentWorkflowTaskState>,
    pub remote_skill_grants: Vec<RemoteSkillGrant>,
}

impl Default for AgentWorkflowState {
    fn default() -> Self {
        Self {
            status: STATUS_IDLE.to_string(),
            final_plan: None,
            review: None,
            tasks: Vec::new(),
            remote_skill_grants: Vec::new(),
        }
    }
}

impl AgentWorkflowState {
    pub fn submit_final_plan(&mut self, plan: impl Into<String>) {
        self.final_plan = Some(plan.into());
        self.status = STATUS_AWAITING_PLAN_APPROVAL.to_string();
    }

    pub fn submit_review(&mut self, review: impl Into<String>) {
        self.review = Some(review.into());
        self.status = STATUS_AWAITING_REVIEW_APPROVAL.to_string();
    }

    pub fn approve_plan(&mut self) {
        self.status = STATUS_IMPLEMENTATION_ALLOWED.to_string();
    }

    pub fn reject_plan(&mut self, reason: Option<&str>) {
        self.status = STATUS_REJECTED.to_string();
        self.review = reason.map(str::to_string);
    }

    pub fn approve_review(&mut self) {
        self.status = STATUS_COMPLETED.to_string();
    }

    pub fn reject_review(&mut self, reason: Option<&str>) {
        self.status = STATUS_IMPLEMENTATION_ALLOWED.to_string();
        self.review = reason.map(str::to_string);
    }

    pub fn grant_remote_skill(
        &mut self,
        skill_ref: impl Into<String>,
        target_agent_role: Option<String>,
        workflow_task_id: Option<String>,
    ) {
        let raw = skill_ref.into();
        let skill_ref = normalize_remote_skill_ref_for_grant(&raw);
        if self
            .remote_skill_grants
            .iter()
            .any(|grant| grant.skill_ref == skill_ref)
        {
            return;
        }
        self.remote_skill_grants.push(RemoteSkillGrant {
            skill_ref,
            target_agent_role,
            workflow_task_id,
        });
    }

    pub fn has_remote_skill_grant(&self, skill_ref: &str) -> bool {
        let skill_ref = normalize_remote_skill_ref_for_grant(skill_ref);
        self.remote_skill_grants
            .iter()
            .any(|grant| grant.skill_ref == skill_ref)
    }

    pub fn upsert_task(&mut self, task: AgentWorkflowTaskState) {
        if let Some(existing) = self
            .tasks
            .iter_mut()
            .find(|existing| existing.id == task.id && existing.agent_role == task.agent_role)
        {
            *existing = task;
        } else {
            self.tasks.push(task);
        }
    }
}

pub fn enabled() -> bool {
    crate::config::config().features.agent_workflow
}

pub fn roles() -> &'static [&'static str] {
    &[
        ROLE_ORCHESTRATOR,
        ROLE_PLAN_AGENT,
        ROLE_PLAN_REVIEWER,
        ROLE_PLAN_FINALIZER,
        ROLE_FRONTEND,
        ROLE_BACKEND,
        ROLE_CODE_REVIEWER,
    ]
}

pub fn is_known_role(role: &str) -> bool {
    roles().contains(&role)
}

pub fn effective_role(session: &Session) -> String {
    session
        .agent_role
        .clone()
        .filter(|role| !role.trim().is_empty())
        .unwrap_or_else(|| {
            if enabled() {
                ROLE_ORCHESTRATOR.to_string()
            } else {
                ROLE_IMPLEMENTER.to_string()
            }
        })
}

pub fn ensure_orchestrator_front_door(session: &mut Session) {
    if !enabled() || session.parent_id.is_some() {
        return;
    }
    session.agent_role = Some(ROLE_ORCHESTRATOR.to_string());
    session
        .agent_workflow_state
        .get_or_insert_with(AgentWorkflowState::default);
}

pub fn canonical_tool_name(name: &str) -> &str {
    match name {
        "communicate" => "swarm",
        "task" | "task_runner" => "subagent",
        "shell_exec" => "bash",
        "file_read" => "read",
        "file_write" => "write",
        "file_edit" => "edit",
        "file_glob" => "glob",
        "file_grep" => "grep",
        "skill" | "Skill" => "skill_manage",
        "todoread" | "todowrite" | "todo_read" | "todo_write" => "todo",
        other => other,
    }
}

pub fn validate_tool_for_role(
    session: &Session,
    name: &str,
    available_tool_names: Option<&HashSet<String>>,
) -> Result<(), String> {
    let role = effective_role(session);
    let resolved = canonical_tool_name(name);
    if let Some(available) = available_tool_names {
        let all = available.iter().cloned().collect::<Vec<_>>();
        let allowed = role_allowed_tools(&role, &all);
        if !allowed.contains(resolved) {
            return Err(format!(
                "Tool '{}' is blocked for workflow role '{}'",
                name, role
            ));
        }
    }
    if non_implementer_blocked_tools().contains(&resolved)
        && matches!(
            role.as_str(),
            ROLE_ORCHESTRATOR | ROLE_PLAN_AGENT | ROLE_PLAN_REVIEWER | ROLE_PLAN_FINALIZER
        )
    {
        return Err(format!(
            "Tool '{}' is blocked for workflow role '{}'",
            name, role
        ));
    }
    if edit_tools().contains(&resolved) && role == ROLE_CODE_REVIEWER {
        return Err(format!(
            "Tool '{}' is blocked for workflow role '{}'",
            name, role
        ));
    }
    if edit_tools().contains(&resolved) && matches!(role.as_str(), ROLE_FRONTEND | ROLE_BACKEND) {
        workflow_allows_implementation_mutation(session)?;
    }
    Ok(())
}

pub fn mutation_tools() -> &'static [&'static str] {
    edit_tools()
}

pub fn edit_tools() -> &'static [&'static str] {
    &[
        "write",
        "edit",
        "multiedit",
        "apply_patch",
        "patch",
        "selfdev",
    ]
}

pub fn non_implementer_blocked_tools() -> &'static [&'static str] {
    &[
        "write",
        "edit",
        "multiedit",
        "apply_patch",
        "patch",
        "bash",
        "bg",
        "selfdev",
    ]
}

pub fn role_allowed_tools(role: &str, all_tools: &[String]) -> HashSet<String> {
    let allow = |names: &[&str]| {
        names
            .iter()
            .filter(|name| all_tools.iter().any(|tool| tool == **name))
            .map(|name| (*name).to_string())
            .collect::<HashSet<_>>()
    };
    match role {
        ROLE_ORCHESTRATOR => allow(&[
            "subagent",
            "todo",
            "skill_manage",
            "agent_workflow",
            "swarm",
            "communicate",
            "read",
            "read_file",
            "grep",
            "glob",
            "ls",
            "agentgrep",
            "codebase_search",
            "codesearch",
            "commit_lineage_search",
            "session_search",
            "conversation_search",
            "memory",
            "mcp",
            "websearch",
            "webfetch",
        ]),
        ROLE_PLAN_AGENT | ROLE_PLAN_REVIEWER | ROLE_PLAN_FINALIZER => allow(&[
            "read",
            "read_file",
            "grep",
            "glob",
            "ls",
            "agentgrep",
            "codebase_search",
            "codesearch",
            "commit_lineage_search",
            "session_search",
            "memory",
            "mcp",
            "swarm",
            "communicate",
            "skill_manage",
            "todo",
        ]),
        ROLE_FRONTEND | ROLE_BACKEND => all_tools
            .iter()
            .filter(|tool| {
                !matches!(
                    tool.as_str(),
                    "subagent" | "task" | "todowrite" | "todoread" | "selfdev"
                )
            })
            .cloned()
            .collect(),
        ROLE_CODE_REVIEWER => allow(&[
            "read",
            "read_file",
            "grep",
            "glob",
            "ls",
            "agentgrep",
            "codebase_search",
            "codesearch",
            "commit_lineage_search",
            "session_search",
            "conversation_search",
            "memory",
            "mcp",
            "skill_manage",
            "swarm",
            "communicate",
            "bash",
            "browser",
            "webfetch",
            "websearch",
        ]),
        _ => all_tools.iter().cloned().collect(),
    }
}

pub fn role_can_spawn(parent_state: Option<&AgentWorkflowState>, role: &str) -> Result<(), String> {
    if !is_known_role(role) {
        return Err(format!(
            "unknown agent role '{}'. Allowed: {}",
            role,
            roles().join(", ")
        ));
    }
    let status = parent_state
        .map(|state| state.status.as_str())
        .unwrap_or(STATUS_IDLE);
    if matches!(role, ROLE_FRONTEND | ROLE_BACKEND)
        && !matches!(
            status,
            STATUS_IMPLEMENTATION_ALLOWED | STATUS_IMPLEMENTING | STATUS_REVIEWING
        )
    {
        return Err("implementation agent blocked until /approve-plan".to_string());
    }
    if role == ROLE_CODE_REVIEWER
        && !matches!(
            status,
            STATUS_IMPLEMENTATION_ALLOWED | STATUS_IMPLEMENTING | STATUS_REVIEWING
        )
    {
        return Err("code-reviewer blocked until implementation is approved".to_string());
    }
    Ok(())
}

pub fn role_status_for_start(role: &str) -> &'static str {
    match role {
        ROLE_PLAN_AGENT | ROLE_PLAN_REVIEWER | ROLE_PLAN_FINALIZER => STATUS_PLANNING,
        ROLE_FRONTEND | ROLE_BACKEND => STATUS_IMPLEMENTING,
        ROLE_CODE_REVIEWER => STATUS_REVIEWING,
        _ => STATUS_IDLE,
    }
}

pub fn role_status_for_complete(role: &str) -> &'static str {
    match role {
        ROLE_PLAN_FINALIZER => STATUS_AWAITING_PLAN_APPROVAL,
        ROLE_CODE_REVIEWER => STATUS_AWAITING_REVIEW_APPROVAL,
        ROLE_FRONTEND | ROLE_BACKEND => STATUS_IMPLEMENTING,
        ROLE_PLAN_AGENT | ROLE_PLAN_REVIEWER => STATUS_PLANNING,
        _ => STATUS_IDLE,
    }
}

pub fn workflow_prompt_for_role(role: &str) -> String {
    let mut prompt = format!(
        "# Agent Workflow\n\nCurrent agent role: `{role}`. Keep orchestration artifacts compact. Do not paste full child transcripts or long logs into the parent session.\n\nAll workflow agents obey these invariants:\n- Think Before Coding: state assumptions; if unclear, ask or return `needs_clarification`; never guess silently.\n- Simplicity First: implement the smallest solution that satisfies the request; avoid unused abstraction or extra configurability.\n- Surgical Changes: touch only files required by the task; match local style; do not refactor unrelated code.\n- Goal-Driven Execution: define success criteria; verify before claiming completion; report any verification not run.\n"
    );
    match role {
        ROLE_ORCHESTRATOR => prompt.push_str(
            "\nOrchestrator contract:\n- You are the default user-facing agent. If the user mentions another agent, still receive the prompt first, then delegate.\n- When requirements are ambiguous or risky, ask concise clarifying questions before delegation.\n- Before each delegation, briefly cover `understanding`, `assumptions`, `critique`, `simpler_option`, and `delegation`.\n- Use `plan-agent`, `plan-reviewer`, then `plan-finalizer` before implementation. Use stable `workflow_task_id`s so child sessions resume.\n- Submit the final plan with `agent_workflow submit_final_plan`, then stop for `/approve-plan`.\n- After implementation agents finish, delegate `code-reviewer`, summarize `result_summary`, `verification`, `risks`, and `accept_or_rework`, submit review, then stop for `/approve-review`.\n- Aggregate child artifacts and communication reports only; inspect full transcripts only on explicit debug need.\n",
        ),
        ROLE_PLAN_AGENT | ROLE_PLAN_REVIEWER | ROLE_PLAN_FINALIZER => prompt.push_str(
            "\nPlanning contract: produce compact planning artifacts only. If scope is unclear, return `needs_clarification` with exact questions. Do not edit files.\n",
        ),
        ROLE_FRONTEND | ROLE_BACKEND => prompt.push_str(
            "\nImplementation contract: implement only assigned scope after plan approval. Keep diffs surgical. Report assumptions, scope control, files changed, commands run, validation, risks, and handoff.\n",
        ),
        ROLE_CODE_REVIEWER => prompt.push_str(
            "\nReview contract: review and verify; do not edit files. Lead with findings, test results, residual risk, and accept_or_rework recommendation.\n",
        ),
        _ => {}
    }
    prompt
}

pub fn workflow_allows_implementation_mutation(session: &Session) -> Result<(), String> {
    let Some(parent_id) = session.parent_id.as_deref() else {
        return Err("workflow implementation agents require a parent orchestrator session".into());
    };
    let parent = Session::load(parent_id)
        .map_err(|err| format!("failed to load workflow parent session: {err}"))?;
    let status = parent
        .agent_workflow_state
        .as_ref()
        .map(|state| state.status.as_str())
        .unwrap_or(STATUS_IDLE);
    if matches!(
        status,
        STATUS_IMPLEMENTATION_ALLOWED | STATUS_IMPLEMENTING | STATUS_REVIEWING
    ) {
        Ok(())
    } else {
        Err(format!(
            "edit tools blocked for workflow status '{}'; use /approve-plan or /reject-review first",
            status
        ))
    }
}

pub fn artifact_prompt(role: &str, task_id: Option<&str>) -> String {
    format!(
        "\n\n<agent_workflow_contract>\nrole: {role}\ntask_id: {}\nReturn only a compact artifact for the orchestrator. Include: summary, assumptions, scope_control, files_changed, commands_run, validation, risks, next_action, handoff. If ambiguity blocks safe work, set next_action to `needs_clarification` and include exact questions; do not guess. Do not paste full logs or full transcripts; store details in this child session.\n</agent_workflow_contract>",
        task_id.unwrap_or("unassigned")
    )
}

pub fn cap_artifact(text: &str) -> (String, bool) {
    if text.len() <= ARTIFACT_MAX_CHARS {
        return (text.to_string(), false);
    }
    let mut capped = text[..ARTIFACT_MAX_CHARS].to_string();
    capped.push_str(
        "\n\n[agent_workflow] artifact truncated; inspect child session for full detail.",
    );
    (capped, true)
}

pub fn artifact_summary(text: &str) -> String {
    crate::util::truncate_str(text.trim(), 2_000).to_string()
}

pub fn custom_skill_role_for_root(root: &std::path::Path) -> Option<String> {
    let role = root.file_name()?.to_str()?.to_string();
    let parent = root.parent()?.file_name()?.to_str()?;
    (parent == "agent-skills" && is_known_role(&role)).then_some(role)
}

pub fn find_existing_child_session(parent_id: &str, role: &str, task_id: &str) -> Option<String> {
    let sessions_dir = crate::storage::jcode_dir().ok()?.join("sessions");
    let entries = std::fs::read_dir(sessions_dir).ok()?;
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(session) = Session::load_startup_stub(stem).or_else(|_| Session::load(stem)) else {
            continue;
        };
        if session.parent_id.as_deref() == Some(parent_id)
            && session.agent_role.as_deref() == Some(role)
            && session.workflow_task_id.as_deref() == Some(task_id)
        {
            matches.push((session.updated_at, session.id));
        }
    }
    matches.sort_by(|left, right| right.0.cmp(&left.0));
    matches.into_iter().map(|(_, id)| id).next()
}

pub fn project_agent_skill_roots(working_dir: Option<&std::path::Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(jcode_dir) = crate::storage::jcode_dir() {
        roots.push(jcode_dir.join("agent-skills"));
    }
    if let Some(working_dir) = working_dir {
        roots.push(working_dir.join(".jcode").join("agent-skills"));
    }
    roots
}

pub fn workflow_command(input: &str, session: &mut Session) -> Option<String> {
    let trimmed = input.trim();
    let mut state = session.agent_workflow_state.clone().unwrap_or_default();
    let message = if trimmed == "/approve-plan" {
        state.approve_plan();
        "Plan approved. Implementation agents are now allowed.".to_string()
    } else if let Some(reason) = trimmed.strip_prefix("/reject-plan") {
        state.reject_plan(Some(reason.trim()).filter(|value| !value.is_empty()));
        "Plan rejected. Workflow returned to planning.".to_string()
    } else if trimmed == "/approve-review" {
        state.approve_review();
        "Review approved. Workflow complete.".to_string()
    } else if let Some(reason) = trimmed.strip_prefix("/reject-review") {
        state.reject_review(Some(reason.trim()).filter(|value| !value.is_empty()));
        "Review rejected. Implementation is unlocked for fixes.".to_string()
    } else if let Some(skill_ref) = trimmed.strip_prefix("/approve-skill") {
        let skill_ref = skill_ref.trim();
        if skill_ref.is_empty() {
            return Some("Usage: /approve-skill <skills.sh id-or-url>".to_string());
        }
        let skill_ref = normalize_remote_skill_ref_for_grant(skill_ref);
        state.grant_remote_skill(skill_ref.clone(), None, None);
        format!("Remote skill approved for this workflow: {skill_ref}")
    } else {
        return None;
    };
    session.agent_workflow_state = Some(state);
    if let Err(error) = session.save() {
        return Some(format!("Workflow command applied but save failed: {error}"));
    }
    Some(message)
}

pub fn normalize_remote_skill_ref_for_grant(input: &str) -> String {
    let trimmed = input.trim().trim_end_matches('/');
    for prefix in ["https://www.skills.sh/", "https://skills.sh/"] {
        if let Some(path) = trimmed.strip_prefix(prefix) {
            return path.trim_start_matches('/').to_string();
        }
    }
    trimmed.trim_start_matches('/').to_string()
}

pub fn render_status(session: &Session) -> String {
    let state = session.agent_workflow_state.clone().unwrap_or_default();
    let mut output = String::new();
    output.push_str(&format!("status: {}\n", state.status));
    output.push_str(&format!("tasks: {}\n", state.tasks.len()));
    if let Some(plan) = state.final_plan.as_deref() {
        output.push_str(&format!(
            "final_plan: {}\n",
            crate::util::truncate_str(plan, 500)
        ));
    }
    if let Some(review) = state.review.as_deref() {
        output.push_str(&format!(
            "review: {}\n",
            crate::util::truncate_str(review, 500)
        ));
    }
    if !state.tasks.is_empty() {
        output.push_str("task_states:\n");
        for task in state.tasks {
            output.push_str(&format!(
                "- id={} role={} session={} status={}\n",
                task.id, task.agent_role, task.session_id, task.status
            ));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn workflow_command_approvals_mutate_state() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("JCODE_HOME", temp.path().as_os_str());
        let mut session = Session::create_with_id("workflow_cmd".into(), None, None);

        let message = workflow_command(
            "/approve-skill https://www.skills.sh/ui-ux-pro",
            &mut session,
        )
        .unwrap();
        assert!(message.contains("ui-ux-pro"));
        assert!(
            session
                .agent_workflow_state
                .as_ref()
                .unwrap()
                .has_remote_skill_grant("ui-ux-pro")
        );

        workflow_command("/approve-plan", &mut session).unwrap();
        assert_eq!(
            session.agent_workflow_state.as_ref().unwrap().status,
            STATUS_IMPLEMENTATION_ALLOWED
        );
        workflow_command("/approve-review", &mut session).unwrap();
        assert_eq!(
            session.agent_workflow_state.as_ref().unwrap().status,
            STATUS_COMPLETED
        );
    }

    #[test]
    fn workflow_implementation_mutation_requires_open_parent_gate() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("JCODE_HOME", temp.path().as_os_str());

        let mut parent = Session::create_with_id("workflow_parent".into(), None, None);
        parent.agent_workflow_state = Some(AgentWorkflowState::default());
        parent.save().unwrap();

        let mut child = Session::create_with_id(
            "workflow_child".into(),
            Some(parent.id.clone()),
            Some("frontend".into()),
        );
        child.agent_role = Some(ROLE_FRONTEND.to_string());
        child.workflow_task_id = Some("task-ui".to_string());
        child.save().unwrap();

        assert!(workflow_allows_implementation_mutation(&child).is_err());

        let mut state = parent.agent_workflow_state.clone().unwrap();
        state.approve_plan();
        parent.agent_workflow_state = Some(state);
        parent.save().unwrap();
        assert!(workflow_allows_implementation_mutation(&child).is_ok());

        let mut state = parent.agent_workflow_state.clone().unwrap();
        state.submit_review("review");
        parent.agent_workflow_state = Some(state);
        parent.save().unwrap();
        assert!(workflow_allows_implementation_mutation(&child).is_err());
    }

    #[test]
    fn workflow_role_tool_matrix_keeps_reviewer_read_only_except_shell() {
        let tools = ["read", "bash", "write", "skill_manage", "swarm"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();

        let reviewer = role_allowed_tools(ROLE_CODE_REVIEWER, &tools);
        assert!(reviewer.contains("bash"));
        assert!(reviewer.contains("read"));
        assert!(reviewer.contains("swarm"));
        assert!(!reviewer.contains("write"));

        let planner = role_allowed_tools(ROLE_PLAN_AGENT, &tools);
        assert!(planner.contains("read"));
        assert!(planner.contains("swarm"));
        assert!(!planner.contains("bash"));
        assert!(!planner.contains("write"));
    }

    #[test]
    fn workflow_front_door_forces_root_session_to_orchestrator() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("JCODE_HOME", temp.path().as_os_str());
        let _enabled =
            EnvVarGuard::set("JCODE_AGENT_WORKFLOW_ENABLED", std::ffi::OsStr::new("true"));
        crate::config::invalidate_config_cache();

        let mut root = Session::create_with_id("workflow_root".into(), None, None);
        root.agent_role = Some(ROLE_BACKEND.to_string());
        ensure_orchestrator_front_door(&mut root);
        assert_eq!(root.agent_role.as_deref(), Some(ROLE_ORCHESTRATOR));
        assert!(root.agent_workflow_state.is_some());

        let mut child =
            Session::create_with_id("workflow_child_role".into(), Some(root.id.clone()), None);
        child.agent_role = Some(ROLE_FRONTEND.to_string());
        ensure_orchestrator_front_door(&mut child);
        assert_eq!(child.agent_role.as_deref(), Some(ROLE_FRONTEND));
    }

    #[test]
    fn workflow_contract_prompts_require_clarification_and_structured_review() {
        let orchestrator = workflow_prompt_for_role(ROLE_ORCHESTRATOR);
        assert!(orchestrator.contains("Think Before Coding"));
        assert!(orchestrator.contains("Simplicity First"));
        assert!(orchestrator.contains("Surgical Changes"));
        assert!(orchestrator.contains("Goal-Driven Execution"));
        assert!(orchestrator.contains("understanding"));
        assert!(orchestrator.contains("critique"));
        assert!(orchestrator.contains("simpler_option"));
        assert!(orchestrator.contains("accept_or_rework"));

        let artifact = artifact_prompt(ROLE_BACKEND, Some("task-api"));
        assert!(artifact.contains("assumptions"));
        assert!(artifact.contains("scope_control"));
        assert!(artifact.contains("needs_clarification"));
    }

    #[test]
    fn workflow_tool_validation_resolves_communication_aliases() {
        let mut session = Session::create_with_id("workflow_alias".into(), None, None);
        session.agent_role = Some(ROLE_ORCHESTRATOR.to_string());
        session.agent_workflow_state = Some(AgentWorkflowState::default());
        let available = ["swarm", "read", "write"]
            .into_iter()
            .map(str::to_string)
            .collect::<HashSet<_>>();

        assert!(validate_tool_for_role(&session, "communicate", Some(&available)).is_ok());
        assert!(validate_tool_for_role(&session, "write", Some(&available)).is_err());
    }
}
