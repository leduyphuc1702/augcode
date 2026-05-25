//! Skill tool - load, list, reload, and read skills

use super::{Tool, ToolContext, ToolOutput};
use crate::skill::{SkillLookup, SkillRegistry};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct SkillTool {
    registry: Arc<RwLock<SkillRegistry>>,
}

impl SkillTool {
    pub fn new(registry: Arc<RwLock<SkillRegistry>>) -> Self {
        Self { registry }
    }
}

#[derive(Deserialize)]
struct SkillInput {
    /// Action to perform: load (default), list, reload, reload_all, read
    #[serde(default = "default_action")]
    action: String,
    /// Skill name (required for load, reload, read)
    #[serde(alias = "skill")]
    #[serde(default)]
    name: Option<String>,
    /// Optional Claude-compatible Skill wrapper argument. The skill loader only
    /// needs to load the prompt, so args are currently accepted and ignored.
    #[serde(default)]
    args: Option<String>,
    /// Optional task intent/path hint used by the router when validating scoped skills.
    #[serde(default)]
    intent: Option<String>,
    /// Remote skills.sh id or URL for remote_preview/remote_read.
    #[serde(default)]
    url: Option<String>,
}

fn default_action() -> String {
    "load".to_string()
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn description(&self) -> &str {
        "Manage skills."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["load", "list", "reload", "reload_all", "read", "verify", "remote_preview", "remote_read"],
                    "description": "Action."
                },
                "name": {
                    "type": "string",
                    "description": "Skill name."
                },
                "url": {
                    "type": "string",
                    "description": "skills.sh skill id or URL."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: SkillInput = serde_json::from_value(input)?;
        let action_label = params.action.clone();
        let name_label = params.name.clone().unwrap_or_else(|| "<none>".to_string());
        let _args = params.args.as_deref();
        let intent = params.intent.clone();

        match params.action.as_str() {
            "load" => {
                self.load_skill(
                    params.name,
                    ctx.working_dir.as_deref(),
                    ctx.allowed_tools.as_ref(),
                    intent.as_deref(),
                    ctx.agent_role.as_deref(),
                )
                .await
            }
            "list" => self.list_skills().await,
            "reload" => self.reload_skill(params.name).await,
            "reload_all" => self.reload_all_skills(ctx.working_dir.as_deref()).await,
            "verify" => self.verify_skills(ctx.agent_role.as_deref()).await,
            "remote_preview" => self.remote_preview(params.url.or(params.name)).await,
            "remote_read" => self.remote_read(params.url.or(params.name), &ctx.session_id).await,
            "read" => {
                self.read_skill(
                    params.name,
                    ctx.working_dir.as_deref(),
                    ctx.allowed_tools.as_ref(),
                    intent.as_deref(),
                    ctx.agent_role.as_deref(),
                )
                .await
            }
            _ => Ok(ToolOutput::new(format!(
                "Unknown action: {}. Use 'load', 'list', 'reload', 'reload_all', 'read', 'verify', 'remote_preview', or 'remote_read'.",
                params.action
            ))),
        }
        .map_err(|err| {
            crate::logging::warn(&format!(
                "[tool:skill_manage] action failed action={} skill={} session_id={} error={}",
                action_label, name_label, ctx.session_id, err
            ));
            err
        })
    }
}

impl SkillTool {
    async fn load_skill(
        &self,
        name: Option<String>,
        working_dir: Option<&std::path::Path>,
        allowed_tools: Option<&std::collections::HashSet<String>>,
        intent: Option<&str>,
        agent_role: Option<&str>,
    ) -> Result<ToolOutput> {
        let name = normalize_skill_name(name, "load")?;

        let registry = self.registry.read().await;
        let skill = resolve_skill(&registry, &name)?;
        validate_router_access(
            skill,
            working_dir,
            allowed_tools,
            intent,
            agent_role,
            "load",
        )?;

        let base_dir = skill
            .path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| ".".to_string());

        Ok(ToolOutput::new(format!(
            "## Skill: {}\n\n**Base directory**: {}\n\n{}",
            skill.name,
            base_dir,
            if crate::skill_router::enabled() {
                skill.get_untrusted_prompt()
            } else {
                skill.get_prompt()
            }
        ))
        .with_title(format!("skill: {}", skill.name)))
    }

    async fn list_skills(&self) -> Result<ToolOutput> {
        let registry = self.registry.read().await;
        let skills = registry.list();

        if skills.is_empty() {
            return Ok(ToolOutput::new(
                "No skills available.\n\n\
                Skills are loaded from:\n\
                - ~/.claude/skills/<skill-name>/SKILL.md\n\
                - ./.claude/skills/<skill-name>/SKILL.md\n\n\
                Create a SKILL.md file with YAML frontmatter:\n\
                ---\n\
                name: my-skill\n\
                description: What this skill does\n\
                allowed-tools: bash, read, write\n\
                ---\n\n\
                # Skill content here",
            )
            .with_title("Skills: None available"));
        }

        let mut output = format!("Available skills: {}\n\n", skills.len());

        for skill in skills {
            output.push_str(&format!("## /{}\n", skill.name));
            output.push_str(&format!("  {}\n", skill.description));
            output.push_str(&format!("  ID: {}\n", skill.id));
            output.push_str(&format!("  Source: {}\n", skill.manifest.source_kind));
            output.push_str(&format!(
                "  Invocation: {}\n",
                skill.manifest.invocation_mode
            ));
            output.push_str(&format!("  Path: {}\n", skill.path.display()));
            if let Some(ref tools) = skill.allowed_tools {
                output.push_str(&format!("  Tools: {}\n", tools.join(", ")));
            }
            output.push('\n');
        }

        Ok(ToolOutput::new(output).with_title("Skills: List"))
    }

    async fn reload_skill(&self, name: Option<String>) -> Result<ToolOutput> {
        let name = normalize_skill_name(name, "reload")?;

        let mut registry = self.registry.write().await;

        match registry.reload(&name) {
            Ok(true) => {
                // Re-read to get updated info
                if let Some(skill) = registry.get(&name) {
                    Ok(ToolOutput::new(format!(
                        "Reloaded skill '{}'\n\nDescription: {}\nPath: {}",
                        name,
                        skill.description,
                        skill.path.display()
                    ))
                    .with_title(format!("Skills: Reloaded {}", name)))
                } else {
                    Ok(ToolOutput::new(format!("Reloaded skill '{}'", name))
                        .with_title(format!("Skills: Reloaded {}", name)))
                }
            }
            Ok(false) => Ok(ToolOutput::new(format!(
                "Skill '{}' not found or was deleted.\n\nUse 'list' to see available skills.",
                name
            ))
            .with_title("Skills: Not found")),
            Err(e) => {
                crate::logging::warn(&format!(
                    "[tool:skill_manage] reload failed skill={} error={}",
                    name, e
                ));
                Ok(
                    ToolOutput::new(format!("Failed to reload skill '{}': {}", name, e))
                        .with_title("Skills: Reload failed"),
                )
            }
        }
    }

    async fn reload_all_skills(&self, working_dir: Option<&std::path::Path>) -> Result<ToolOutput> {
        let mut registry = self.registry.write().await;

        match registry.reload_all_for_working_dir(working_dir) {
            Ok(count) => {
                let skills = registry.list();
                let mut output = format!("Reloaded {} skills\n\n", count);

                for skill in skills {
                    output.push_str(&format!("- /{}: {}\n", skill.name, skill.description));
                }

                Ok(ToolOutput::new(output).with_title(format!("Skills: Reloaded {}", count)))
            }
            Err(e) => {
                crate::logging::warn(&format!(
                    "[tool:skill_manage] reload_all failed error={}",
                    e
                ));
                Ok(ToolOutput::new(format!("Failed to reload skills: {}", e))
                    .with_title("Skills: Reload failed"))
            }
        }
    }

    async fn read_skill(
        &self,
        name: Option<String>,
        working_dir: Option<&std::path::Path>,
        allowed_tools: Option<&std::collections::HashSet<String>>,
        intent: Option<&str>,
        agent_role: Option<&str>,
    ) -> Result<ToolOutput> {
        let name = normalize_skill_name(name, "read")?;

        let registry = self.registry.read().await;

        if let SkillLookup::Found(skill) = registry.lookup(&name) {
            validate_router_access(
                skill,
                working_dir,
                allowed_tools,
                intent,
                agent_role,
                "read",
            )?;
            let mut output = format!("# Skill: {}\n\n", skill.name);
            output.push_str(&format!("**ID:** {}\n", skill.id));
            output.push_str(&format!("**Description:** {}\n", skill.description));
            output.push_str(&format!("**Path:** {}\n", skill.path.display()));
            output.push_str(&format!("**Source:** {}\n", skill.manifest.source_kind));
            output.push_str(&format!(
                "**Invocation:** {}\n",
                skill.manifest.invocation_mode
            ));
            if let Some(ref tools) = skill.allowed_tools {
                output.push_str(&format!("**Allowed tools:** {}\n", tools.join(", ")));
            }
            output.push_str("\n---\n\n");
            output.push_str(&skill.content);

            Ok(ToolOutput::new(output).with_title(format!("Skills: {}", name)))
        } else if let SkillLookup::Ambiguous(matches) = registry.lookup(&name) {
            Ok(ToolOutput::new(format_ambiguity(&name, matches)).with_title("Skills: Ambiguous"))
        } else {
            Ok(ToolOutput::new(format!(
                "Skill '{}' not found.\n\nUse 'list' to see available skills.",
                name
            ))
            .with_title("Skills: Not found"))
        }
    }

    async fn verify_skills(&self, agent_role: Option<&str>) -> Result<ToolOutput> {
        let registry = self.registry.read().await;
        let role = agent_role.unwrap_or(crate::agent_workflow::ROLE_IMPLEMENTER);
        let mut output = format!("Skill verification for role `{role}`:\n\n");
        for skill in registry.list() {
            let role_allowed = skill.manifest.allowed_agents.is_empty()
                || skill
                    .manifest
                    .allowed_agents
                    .iter()
                    .any(|allowed| allowed == role);
            let skills_sh = skill
                .manifest
                .skills_sh_id
                .as_deref()
                .or(skill.manifest.skills_sh_url.as_deref())
                .unwrap_or("<none>");
            output.push_str(&format!(
                "- /{} id={} source={} skills_sh={} role_allowed={} invocation={}\n",
                skill.name,
                skill.id,
                skill.manifest.source_kind,
                skills_sh,
                role_allowed,
                skill.manifest.invocation_mode
            ));
        }
        Ok(ToolOutput::new(output).with_title("Skills: Verify"))
    }

    async fn remote_preview(&self, skill_ref: Option<String>) -> Result<ToolOutput> {
        let skill_ref = normalize_remote_skill_ref(skill_ref)?;
        ensure_skills_sh_ref(&skill_ref)?;
        let text = fetch_remote_skill(&skill_ref, 8_000).await?;
        let preview = crate::util::truncate_str(&text, 2_000);
        Ok(ToolOutput::new(format!(
            "Remote skill preview: {}\n\n{}\n\nUse `/approve-skill {}` before `remote_read`.",
            skill_ref, preview, skill_ref
        ))
        .with_title("Skills: Remote preview"))
    }

    async fn remote_read(&self, skill_ref: Option<String>, session_id: &str) -> Result<ToolOutput> {
        let skill_ref = normalize_remote_skill_ref(skill_ref)?;
        ensure_skills_sh_ref(&skill_ref)?;
        let session = crate::session::Session::load(session_id)?;
        let approved = session
            .agent_workflow_state
            .as_ref()
            .map(|state| state.has_remote_skill_grant(&skill_ref))
            .unwrap_or(false)
            || session
                .parent_id
                .as_deref()
                .and_then(|parent_id| crate::session::Session::load(parent_id).ok())
                .and_then(|parent| parent.agent_workflow_state)
                .map(|state| state.has_remote_skill_grant(&skill_ref))
                .unwrap_or(false);
        if crate::config::config().skills.remote_read_policy == "approve" && !approved {
            anyhow::bail!(
                "Remote skill '{}' requires approval. Run /approve-skill {} first.",
                skill_ref,
                skill_ref
            );
        }
        let text = fetch_remote_skill(&skill_ref, 64_000).await?;
        Ok(ToolOutput::new(format!(
            "# Remote Skill (untrusted)\n\nSource: {}\n\n{}",
            skill_ref, text
        ))
        .with_title("Skills: Remote read"))
    }
}

fn validate_router_access(
    skill: &crate::skill::Skill,
    working_dir: Option<&std::path::Path>,
    allowed_tools: Option<&std::collections::HashSet<String>>,
    intent: Option<&str>,
    agent_role: Option<&str>,
    action: &str,
) -> Result<()> {
    if !crate::skill_router::enabled() {
        return Ok(());
    }
    let agent = crate::skill_router::AgentProfile::from_allowed_tools(
        "skill_manage",
        agent_role.unwrap_or(crate::agent_workflow::ROLE_IMPLEMENTER),
        allowed_tools,
    );
    crate::skill_router::validate_explicit_load_for_intent(
        &skill.manifest,
        working_dir,
        &agent,
        intent,
    )
    .map_err(|reason| {
        anyhow::anyhow!(
            "Skill '{}' blocked by router during {}: {}",
            skill.name,
            action,
            reason
        )
    })
}

fn normalize_remote_skill_ref(skill_ref: Option<String>) -> Result<String> {
    let skill_ref =
        skill_ref.ok_or_else(|| anyhow::anyhow!("'url' or 'name' is required for remote skill"))?;
    let skill_ref = skill_ref.trim();
    if skill_ref.is_empty() {
        anyhow::bail!("remote skill ref cannot be empty");
    }
    Ok(crate::agent_workflow::normalize_remote_skill_ref_for_grant(
        skill_ref,
    ))
}

fn ensure_skills_sh_ref(skill_ref: &str) -> Result<()> {
    if skill_ref.starts_with("https://www.skills.sh/")
        || skill_ref.starts_with("https://skills.sh/")
        || (!skill_ref.starts_with("http://") && !skill_ref.starts_with("https://"))
    {
        return Ok(());
    }
    anyhow::bail!("remote skill refs must be skills.sh ids or https://skills.sh URLs")
}

async fn fetch_remote_skill(skill_ref: &str, max_bytes: usize) -> Result<String> {
    let url = if skill_ref.starts_with("http://") || skill_ref.starts_with("https://") {
        skill_ref.to_string()
    } else {
        format!(
            "https://www.skills.sh/{}",
            skill_ref.trim_start_matches('/')
        )
    };
    let response = crate::provider::shared_http_client()
        .get(&url)
        .header(reqwest::header::USER_AGENT, "jcode-skill-router")
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("remote skill fetch failed: HTTP {}", response.status());
    }
    let text = response.text().await?;
    Ok(crate::util::truncate_str(&text, max_bytes).to_string())
}

fn resolve_skill<'a>(registry: &'a SkillRegistry, name: &str) -> Result<&'a crate::skill::Skill> {
    match registry.lookup(name) {
        SkillLookup::Found(skill) => Ok(skill),
        SkillLookup::Ambiguous(matches) => anyhow::bail!("{}", format_ambiguity(name, matches)),
        SkillLookup::Missing => anyhow::bail!("Skill '{}' not found", name),
    }
}

fn format_ambiguity(name: &str, matches: Vec<&crate::skill::Skill>) -> String {
    let mut output = format!(
        "Skill '{}' is ambiguous. Use a skill ID or source:scope:name.\n\n",
        name
    );
    for skill in matches {
        let scope = skill
            .manifest
            .scope_dir
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "global".to_string());
        output.push_str(&format!(
            "- {} | source={} scope={} name={}\n",
            skill.id, skill.manifest.source_kind, scope, skill.manifest.canonical_name
        ));
    }
    output
}

fn normalize_skill_name(name: Option<String>, action: &str) -> Result<String> {
    let name = name.ok_or_else(|| anyhow::anyhow!("'name' is required for {} action", action))?;
    let trimmed = name.trim().trim_start_matches('/').to_string();
    if trimmed.is_empty() {
        anyhow::bail!("'name' is required for {} action", action);
    }
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)]

    use super::*;

    struct EnvVarGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
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

    fn create_test_tool() -> SkillTool {
        let registry = Arc::new(RwLock::new(SkillRegistry::default()));
        SkillTool::new(registry)
    }

    fn create_test_tool_with_skill(name: &str) -> (SkillTool, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let skill_dir = temp_dir.path().join(".jcode").join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: Test skill\n---\n\n# Test Skill\n\nUse this test skill."
            ),
        )
        .unwrap();

        let registry = SkillRegistry::load_for_working_dir(Some(temp_dir.path())).unwrap();
        let tool = SkillTool::new(Arc::new(RwLock::new(registry)));
        (tool, temp_dir)
    }

    fn create_test_tool_with_duplicate_skill(name: &str) -> (SkillTool, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        for scope in [".jcode", ".claude"] {
            let skill_dir = temp_dir.path().join(scope).join("skills").join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!(
                    "---\nname: {name}\ndescription: Test skill {scope}\n---\n\n# Test Skill\n\nUse this test skill."
                ),
            )
            .unwrap();
        }

        let registry = SkillRegistry::load_for_working_dir(Some(temp_dir.path())).unwrap();
        let tool = SkillTool::new(Arc::new(RwLock::new(registry)));
        (tool, temp_dir)
    }

    fn create_test_tool_with_denied_skill(name: &str) -> (SkillTool, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let skill_dir = temp_dir.path().join(".jcode").join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: Use this skill for blocked router tests\ndenied_agents: implementer\n---\n\n# Test Skill\n\nUse this test skill."
            ),
        )
        .unwrap();

        let registry = SkillRegistry::load_for_working_dir(Some(temp_dir.path())).unwrap();
        let tool = SkillTool::new(Arc::new(RwLock::new(registry)));
        (tool, temp_dir)
    }

    fn create_test_tool_with_required_tool(
        name: &str,
        required_tool: &str,
    ) -> (SkillTool, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let skill_dir = temp_dir.path().join(".jcode").join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: Use this skill for required tool checks\nrequired_tools: {required_tool}\n---\n\n# Test Skill\n\nUse this test skill."
            ),
        )
        .unwrap();

        let registry = SkillRegistry::load_for_working_dir(Some(temp_dir.path())).unwrap();
        let tool = SkillTool::new(Arc::new(RwLock::new(registry)));
        (tool, temp_dir)
    }

    fn create_test_tool_with_nested_skill(name: &str) -> (SkillTool, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let skill_dir = temp_dir.path().join("apps/web/.cursor/skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: Use this skill for React TypeScript component work\n---\n\n# React Skill\n\nUse this test skill."
            ),
        )
        .unwrap();

        let registry = SkillRegistry::load_for_working_dir(Some(temp_dir.path())).unwrap();
        let tool = SkillTool::new(Arc::new(RwLock::new(registry)));
        (tool, temp_dir)
    }

    fn create_test_tool_with_role_custom_skill(
        role: &str,
        name: &str,
    ) -> (SkillTool, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let skill_dir = temp_dir
            .path()
            .join(".jcode")
            .join("agent-skills")
            .join(role)
            .join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: Use this skill for React button UI implementation\n---\n\n# UI Skill\n\nUse this test skill."
            ),
        )
        .unwrap();

        let registry = SkillRegistry::load_for_working_dir(Some(temp_dir.path())).unwrap();
        let tool = SkillTool::new(Arc::new(RwLock::new(registry)));
        (tool, temp_dir)
    }

    fn create_test_context() -> ToolContext {
        ToolContext {
            session_id: "test-session".to_string(),
            message_id: "test-message".to_string(),
            tool_call_id: "test-tool-call".to_string(),
            working_dir: None,
            allowed_tools: None,
            agent_role: None,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: crate::tool::ToolExecutionMode::Direct,
        }
    }

    #[test]
    fn test_tool_name() {
        let tool = create_test_tool();
        assert_eq!(tool.name(), "skill_manage");
    }

    #[test]
    fn test_tool_description() {
        let tool = create_test_tool();
        assert!(tool.description().contains("skill"));
    }

    #[test]
    fn test_parameters_schema() {
        let tool = create_test_tool();
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["action"].is_object());
        assert!(schema["properties"]["name"].is_object());
    }

    #[tokio::test]
    async fn test_list_empty() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "list"});

        let result = tool.execute(input, ctx).await.unwrap();
        assert!(result.output.contains("No skills available"));
    }

    #[tokio::test]
    async fn test_load_missing_name() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "load"});

        let result = tool.execute(input, ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[tokio::test]
    async fn test_load_accepts_skill_alias_and_args() {
        let (tool, _temp_dir) = create_test_tool_with_skill("optimization");
        let ctx = create_test_context();
        let input = json!({"skill": "optimization", "args": "optimize this"});

        let result = tool.execute(input, ctx).await.unwrap();
        assert!(result.output.contains("## Skill: optimization"));
        assert_eq!(result.title.as_deref(), Some("skill: optimization"));
    }

    #[tokio::test]
    async fn test_load_strips_leading_slash_from_name() {
        let (tool, _temp_dir) = create_test_tool_with_skill("optimization");
        let ctx = create_test_context();
        let input = json!({"action": "load", "name": "/optimization"});

        let result = tool.execute(input, ctx).await.unwrap();
        assert!(result.output.contains("## Skill: optimization"));
    }

    #[tokio::test]
    async fn test_load_ambiguous_name_reports_disambiguators() {
        let (tool, _temp_dir) = create_test_tool_with_duplicate_skill("dup-skill");
        let ctx = create_test_context();
        let input = json!({"action": "load", "name": "dup-skill"});

        let result = tool.execute(input, ctx).await;

        let error = result.unwrap_err().to_string();
        assert!(error.contains("ambiguous"));
        assert!(error.contains("source="));
    }

    #[tokio::test]
    async fn test_load_and_read_reject_router_blocked_skill() {
        let _lock = crate::storage::lock_test_env();
        let _env = EnvVarGuard::set("JCODE_PER_AGENT_SKILL_ROUTER_ENABLED", "true");
        let (tool, _temp_dir) = create_test_tool_with_denied_skill("blocked-skill");

        let load = tool
            .execute(
                json!({"action": "load", "name": "blocked-skill"}),
                create_test_context(),
            )
            .await;
        let load_error = load.unwrap_err().to_string();
        assert!(load_error.contains("blocked by router"));
        assert!(load_error.contains("agent-denied"));

        let read = tool
            .execute(
                json!({"action": "read", "name": "blocked-skill"}),
                create_test_context(),
            )
            .await;
        let read_error = read.unwrap_err().to_string();
        assert!(read_error.contains("blocked by router"));
        assert!(read_error.contains("agent-denied"));
    }

    #[tokio::test]
    async fn test_load_rejects_required_tool_not_allowed() {
        let _lock = crate::storage::lock_test_env();
        let _env = EnvVarGuard::set("JCODE_PER_AGENT_SKILL_ROUTER_ENABLED", "true");
        let (tool, temp_dir) = create_test_tool_with_required_tool("needs-bash", "bash");
        let mut ctx = create_test_context();
        ctx.working_dir = Some(temp_dir.path().to_path_buf());
        ctx.allowed_tools = Some(std::collections::HashSet::from(
            ["skill_manage".to_string()],
        ));

        let result = tool
            .execute(json!({"action": "load", "name": "needs-bash"}), ctx)
            .await;

        let error = result.unwrap_err().to_string();
        assert!(error.contains("required-tool-unavailable"));
    }

    #[tokio::test]
    async fn test_read_requires_intent_for_path_scoped_skill() {
        let _lock = crate::storage::lock_test_env();
        let _env = EnvVarGuard::set("JCODE_PER_AGENT_SKILL_ROUTER_ENABLED", "true");
        let (tool, temp_dir) = create_test_tool_with_nested_skill("react-component");
        let mut ctx = create_test_context();
        ctx.working_dir = Some(temp_dir.path().to_path_buf());

        let blocked = tool
            .execute(
                json!({"action": "read", "name": "react-component"}),
                ctx.clone(),
            )
            .await;
        let error = blocked.unwrap_err().to_string();
        assert!(error.contains("path-scope-mismatch"));

        let allowed = tool
            .execute(
                json!({
                    "action": "read",
                    "name": "react-component",
                    "intent": "Edit apps/web/src/Button.tsx"
                }),
                ctx,
            )
            .await
            .unwrap();

        assert!(allowed.output.contains("# Skill: react-component"));
    }

    #[tokio::test]
    async fn workflow_custom_role_skill_routes_only_to_matching_role() {
        let _lock = crate::storage::lock_test_env();
        let _env = EnvVarGuard::set("JCODE_AGENT_WORKFLOW_ENABLED", "true");
        let (tool, temp_dir) =
            create_test_tool_with_role_custom_skill("frontend-agent", "ui-role-skill");
        let manifests = {
            let registry = tool.registry.read().await;
            let skill = registry.get("ui-role-skill").unwrap();
            assert_eq!(
                skill.manifest.source_kind,
                crate::skill_router::SOURCE_CUSTOM_LOCAL
            );
            assert!(
                skill
                    .manifest
                    .allowed_agents
                    .contains(&"frontend-agent".to_string())
            );
            registry.manifests()
        };

        let frontend = crate::skill_router::AgentProfile::from_allowed_tools(
            "frontend",
            "frontend-agent",
            None,
        );
        let backend =
            crate::skill_router::AgentProfile::from_allowed_tools("backend", "backend-agent", None);

        let routed = crate::skill_router::route_for_prompt(
            &manifests,
            "/ui-role-skill build a React button",
            Some(temp_dir.path()),
            &frontend,
            "task-ui",
        );
        assert!(
            routed
                .selected_skills
                .iter()
                .any(|skill| skill.name == "ui-role-skill")
        );

        let blocked = crate::skill_router::route_for_prompt(
            &manifests,
            "/ui-role-skill build a React button",
            Some(temp_dir.path()),
            &backend,
            "task-api",
        );
        assert!(
            blocked
                .blocked_skills
                .iter()
                .any(|rejection| rejection.reason == "agent-not-allowed")
        );
    }

    #[tokio::test]
    async fn workflow_remote_read_requires_approval_before_fetch() {
        let _lock = crate::storage::lock_test_env();
        let temp_home = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("JCODE_HOME", temp_home.path().to_str().unwrap());
        let mut session =
            crate::session::Session::create_with_id("remote_read_workflow".to_string(), None, None);
        session.save().unwrap();

        let tool = create_test_tool();
        let mut ctx = create_test_context();
        ctx.session_id = "remote_read_workflow".to_string();
        let result = tool
            .execute(json!({"action": "remote_read", "name": "ui-ux-pro"}), ctx)
            .await;

        let error = result.unwrap_err().to_string();
        assert!(error.contains("requires approval"));
        assert!(error.contains("/approve-skill ui-ux-pro"));
    }

    #[tokio::test]
    async fn test_read_accepts_skill_id() {
        let (tool, _temp_dir) = create_test_tool_with_skill("id-skill");
        let id = {
            let registry = tool.registry.read().await;
            registry.get("id-skill").unwrap().id.clone()
        };
        let ctx = create_test_context();
        let input = json!({"action": "read", "name": id});

        let result = tool.execute(input, ctx).await.unwrap();

        assert!(result.output.contains("# Skill: id-skill"));
        assert!(result.output.contains("**ID:**"));
    }

    #[tokio::test]
    async fn test_reload_missing_name() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "reload"});

        let result = tool.execute(input, ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[tokio::test]
    async fn test_read_missing_name() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "read"});

        let result = tool.execute(input, ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[tokio::test]
    async fn test_reload_nonexistent() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "reload", "name": "nonexistent"});

        let result = tool.execute(input, ctx).await.unwrap();
        assert!(result.output.contains("not found"));
    }

    #[tokio::test]
    async fn test_unknown_action() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "invalid"});

        let result = tool.execute(input, ctx).await.unwrap();
        assert!(result.output.contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_reload_all() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "reload_all"});

        let result = tool.execute(input, ctx).await.unwrap();
        // The output format is "Reloaded N skills" where N is any number
        // (depends on what skills exist on the system)
        assert!(
            result.output.contains("Reloaded"),
            "Expected 'Reloaded' in output, got: {}",
            result.output
        );
        assert!(
            result.output.contains("skills"),
            "Expected 'skills' in output, got: {}",
            result.output
        );
    }
}
