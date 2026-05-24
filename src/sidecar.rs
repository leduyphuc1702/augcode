//! Lightweight sidecar client for fast, cheap model calls.
//!
//! Used for memory relevance verification and other quick tasks that don't
//! need the full Agent SDK infrastructure.
//!
//! Automatically selects the best available backend:
//! - Local OpenAI-compatible default provider when it points at localhost/private LAN
//! - OpenAI (gpt-5.3-codex-spark) if Codex credentials are available
//! - Claude (claude-haiku-4-5-20241022) if Claude credentials are available

use crate::auth;
use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

/// Fast/cheap OpenAI model used when Codex credentials are available.
pub const SIDECAR_OPENAI_MODEL: &str = "gpt-5.3-codex-spark";
const SIDECAR_OPENAI_OAUTH_FALLBACK_MODEL: &str = "gpt-5.4";
const SIDECAR_OPENAI_OAUTH_FALLBACK_REASONING: &str = "low";

/// Fast/cheap Claude model used when only Claude credentials are available.
const SIDECAR_CLAUDE_MODEL: &str = "claude-haiku-4-5-20241022";

/// OpenAI Responses API
const OPENAI_API_BASE: &str = "https://api.openai.com/v1";
const CHATGPT_API_BASE: &str = "https://chatgpt.com/backend-api/codex";
const OPENAI_RESPONSES_PATH: &str = "responses";
const OPENAI_ORIGINATOR: &str = "codex_cli_rs";

/// Claude Messages API endpoint (with beta=true for OAuth)
const CLAUDE_API_URL: &str = "https://api.anthropic.com/v1/messages?beta=true";

/// User-Agent for OAuth requests (must match Claude CLI format)
const CLAUDE_CLI_USER_AGENT: &str = "claude-cli/1.0.0";

/// Beta headers required for OAuth
const OAUTH_BETA_HEADERS: &str = "oauth-2025-04-20,claude-code-20250219";

/// Claude Code identity block required for OAuth direct API access
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const CLAUDE_CODE_JCODE_NOTICE: &str = "You are jcode, powered by Claude Code. You are a third-party CLI, not the official Claude Code CLI.";

/// Maximum tokens for sidecar responses (keep small for speed/cost)
const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Which backend the sidecar is using
#[derive(Debug, Clone, Copy, PartialEq)]
enum SidecarBackend {
    OpenAI,
    Claude,
    LocalOpenAI,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalOpenAiEndpoint {
    base_url: String,
    auth: LocalOpenAiAuth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalOpenAiAuth {
    None,
    Bearer(String),
    Header { name: String, value: String },
}

impl LocalOpenAiAuth {
    fn apply(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        match self {
            Self::None => Ok(builder),
            Self::Bearer(token) => Ok(builder.header("Authorization", format!("Bearer {}", token))),
            Self::Header { name, value } => {
                let name = HeaderName::from_bytes(name.as_bytes())
                    .context("Invalid local OpenAI-compatible auth header name")?;
                let value = HeaderValue::from_str(value)
                    .context("Invalid local OpenAI-compatible auth header value")?;
                Ok(builder.header(name, value))
            }
        }
    }
}

#[derive(Debug, Clone)]
struct LocalOpenAiCandidate {
    endpoint: LocalOpenAiEndpoint,
    default_model: Option<String>,
    static_models: Vec<String>,
}

/// Lightweight client for fast sidecar calls
#[derive(Clone)]
pub struct Sidecar {
    client: reqwest::Client,
    model: String,
    max_tokens: u32,
    backend: SidecarBackend,
    local_openai: Option<LocalOpenAiEndpoint>,
}

impl Sidecar {
    /// Create a new sidecar client, auto-selecting the best available backend.
    /// Prefers OpenAI (codex-spark) if creds exist, falls back to Claude.
    pub fn new() -> Self {
        let configured_model = crate::config::config().agents.memory_model.clone();
        Self::with_configured_model(configured_model)
    }

    fn with_configured_model(configured_model: Option<String>) -> Self {
        Self::with_configured_model_and_config(configured_model, crate::config::config())
    }

    fn with_configured_model_and_config(
        configured_model: Option<String>,
        cfg: &crate::config::Config,
    ) -> Self {
        let configured_model = configured_model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty());

        let (backend, model, local_openai) = if let Some(model) = configured_model {
            let local = resolve_local_openai_sidecar(Some(model), cfg);
            match known_cloud_sidecar_backend(model).or_else(|| {
                local
                    .is_none()
                    .then(|| heuristic_cloud_sidecar_backend(model))
                    .flatten()
            }) {
                Some(SidecarBackend::OpenAI) => (SidecarBackend::OpenAI, model.to_string(), None),
                Some(SidecarBackend::Claude) => (SidecarBackend::Claude, model.to_string(), None),
                _ if let Some((model, endpoint)) = local => {
                    (SidecarBackend::LocalOpenAI, model, Some(endpoint))
                }
                _ => {
                    crate::logging::warn(&format!(
                        "Ignoring unsupported memory sidecar model override '{}'; expected an OpenAI, Claude, or local OpenAI-compatible model",
                        model
                    ));
                    cloud_sidecar_fallback()
                }
            }
        } else if let Some((model, endpoint)) = resolve_local_openai_sidecar(None, cfg) {
            (SidecarBackend::LocalOpenAI, model, Some(endpoint))
        } else if auth::codex::load_credentials().is_ok() {
            (
                SidecarBackend::OpenAI,
                SIDECAR_OPENAI_MODEL.to_string(),
                None,
            )
        } else if auth::claude::load_credentials().is_ok() {
            (
                SidecarBackend::Claude,
                SIDECAR_CLAUDE_MODEL.to_string(),
                None,
            )
        } else {
            // Default to Claude - will fail on use with a clear error
            (
                SidecarBackend::Claude,
                SIDECAR_CLAUDE_MODEL.to_string(),
                None,
            )
        };

        Self {
            client: crate::provider::shared_http_client(),
            model,
            max_tokens: DEFAULT_MAX_TOKENS,
            backend,
            local_openai,
        }
    }

    /// Return the currently selected sidecar model name.
    pub fn model_name(&self) -> &str {
        &self.model
    }

    /// Return the currently selected backend label.
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            SidecarBackend::OpenAI => "openai",
            SidecarBackend::Claude => "claude",
            SidecarBackend::LocalOpenAI => "local",
        }
    }

    /// Simple completion - send a prompt, get a response.
    /// Routes to the correct API based on the detected backend.
    pub async fn complete(&self, system: &str, user_message: &str) -> Result<String> {
        match self.backend {
            SidecarBackend::OpenAI => self.complete_openai(system, user_message).await,
            SidecarBackend::Claude => self.complete_claude(system, user_message).await,
            SidecarBackend::LocalOpenAI => {
                let endpoint = self
                    .local_openai
                    .as_ref()
                    .context("Local OpenAI-compatible sidecar endpoint missing")?;
                self.complete_local_openai(endpoint, system, user_message)
                    .await
            }
        }
    }

    /// Complete via OpenAI Responses API.
    ///
    /// - Direct API key mode: non-streaming, simple JSON response.
    /// - ChatGPT OAuth mode: streaming SSE (required by chatgpt.com endpoint).
    ///   Prefer codex-spark there too, but fall back to GPT-5.4 with low
    ///   reasoning if spark is unavailable for the current account.
    async fn complete_openai(&self, system: &str, user_message: &str) -> Result<String> {
        let creds = auth::codex::load_credentials()
            .context("Failed to load OpenAI/Codex credentials for sidecar")?;

        let is_chatgpt_mode = !creds.refresh_token.is_empty() || creds.id_token.is_some();
        let base = if is_chatgpt_mode {
            CHATGPT_API_BASE
        } else {
            OPENAI_API_BASE
        };
        let url = format!("{}/{}", base.trim_end_matches('/'), OPENAI_RESPONSES_PATH);

        let (primary_model, primary_reasoning) =
            resolve_openai_request_model(&self.model, is_chatgpt_mode);

        match self
            .complete_openai_with_model(
                &url,
                creds.access_token.as_str(),
                creds.account_id.as_deref(),
                is_chatgpt_mode,
                system,
                user_message,
                primary_model,
                primary_reasoning,
            )
            .await
        {
            Ok(text) => {
                crate::provider::clear_model_unavailable_for_account(primary_model);
                Ok(text)
            }
            Err(OpenAiSidecarError::Api { status, body })
                if is_chatgpt_mode
                    && primary_model == SIDECAR_OPENAI_MODEL
                    && is_openai_model_unavailable(status, &body) =>
            {
                let reason = classify_openai_model_unavailable(status, &body)
                    .unwrap_or_else(|| format!("model denied by OpenAI API (status {})", status));
                crate::provider::record_model_unavailable_for_account(primary_model, &reason);
                crate::logging::info(&format!(
                    "Sidecar fallback: {} unavailable in ChatGPT OAuth mode; retrying {} with reasoning={} ({})",
                    primary_model,
                    SIDECAR_OPENAI_OAUTH_FALLBACK_MODEL,
                    SIDECAR_OPENAI_OAUTH_FALLBACK_REASONING,
                    reason
                ));

                let fallback = self
                    .complete_openai_with_model(
                        &url,
                        creds.access_token.as_str(),
                        creds.account_id.as_deref(),
                        is_chatgpt_mode,
                        system,
                        user_message,
                        SIDECAR_OPENAI_OAUTH_FALLBACK_MODEL,
                        Some(SIDECAR_OPENAI_OAUTH_FALLBACK_REASONING),
                    )
                    .await;

                match fallback {
                    Ok(text) => {
                        crate::provider::clear_model_unavailable_for_account(
                            SIDECAR_OPENAI_OAUTH_FALLBACK_MODEL,
                        );
                        Ok(text)
                    }
                    Err(err) => Err(err.into_anyhow()),
                }
            }
            Err(err) => Err(err.into_anyhow()),
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "OpenAI sidecar call needs endpoint, auth, account, mode, prompts, model, and reasoning effort"
    )]
    async fn complete_openai_with_model(
        &self,
        url: &str,
        access_token: &str,
        account_id: Option<&str>,
        is_chatgpt_mode: bool,
        system: &str,
        user_message: &str,
        model: &str,
        reasoning_effort: Option<&str>,
    ) -> std::result::Result<String, OpenAiSidecarError> {
        let request = build_openai_request(
            model,
            system,
            user_message,
            is_chatgpt_mode,
            reasoning_effort,
        );

        let mut builder = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json");

        if is_chatgpt_mode {
            builder = builder.header("originator", OPENAI_ORIGINATOR);
            if let Some(account_id) = account_id {
                builder = builder.header("chatgpt-account-id", account_id);
            }
        }

        let response = builder
            .json(&request)
            .send()
            .await
            .context("Failed to send request to OpenAI API")
            .map_err(OpenAiSidecarError::other)?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(OpenAiSidecarError::Api { status, body });
        }

        if is_chatgpt_mode {
            collect_openai_sse_text(response)
                .await
                .map_err(OpenAiSidecarError::other)
        } else {
            let result: serde_json::Value = response
                .json()
                .await
                .context("Failed to parse OpenAI API response")
                .map_err(OpenAiSidecarError::other)?;
            extract_openai_response_text(&result).map_err(OpenAiSidecarError::other)
        }
    }

    /// Complete via Claude Messages API
    async fn complete_claude(&self, system: &str, user_message: &str) -> Result<String> {
        let creds = auth::claude::load_credentials()
            .context("Failed to load Claude credentials for sidecar")?;

        let request = ClaudeMessagesRequest {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: build_claude_system_param(system),
            messages: vec![ClaudeMessage {
                role: "user",
                content: user_message,
            }],
        };

        let response = crate::provider::anthropic::apply_oauth_attribution_headers(
            self.client
                .post(CLAUDE_API_URL)
                .header("Authorization", format!("Bearer {}", creds.access_token))
                .header("User-Agent", CLAUDE_CLI_USER_AGENT)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", OAUTH_BETA_HEADERS)
                .header("content-type", "application/json")
                .json(&request),
            &crate::provider::anthropic::new_oauth_request_id(),
        )
        .send()
        .await
        .context("Failed to send request to Claude API")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Claude API error ({}): {}", status, error_text);
        }

        let result: ClaudeMessagesResponse = response
            .json()
            .await
            .context("Failed to parse Claude API response")?;

        let text = result
            .content
            .into_iter()
            .filter_map(|block| {
                if let ClaudeContentBlock::Text { text } = block {
                    Some(text)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");

        Ok(text)
    }

    async fn complete_local_openai(
        &self,
        endpoint: &LocalOpenAiEndpoint,
        system: &str,
        user_message: &str,
    ) -> Result<String> {
        let url = format!(
            "{}/chat/completions",
            endpoint.base_url.trim_end_matches('/')
        );
        let request = build_local_chat_request(&self.model, system, user_message, self.max_tokens);
        let builder = self
            .client
            .post(url)
            .header("Content-Type", "application/json");
        let response = endpoint
            .auth
            .apply(builder)?
            .json(&request)
            .send()
            .await
            .context("Failed to send request to local OpenAI-compatible API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Local OpenAI-compatible API error ({}): {}", status, body);
        }

        let result: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse local OpenAI-compatible response")?;
        extract_local_chat_response_text(&result)
    }

    /// Check if a memory is relevant to the current context
    /// Returns (is_relevant, explanation)
    pub async fn check_relevance(
        &self,
        memory_content: &str,
        current_context: &str,
    ) -> Result<(bool, String)> {
        let system = r#"You are a memory relevance checker. Your job is to determine if a stored memory is relevant to the current context.

Respond in this exact format:
RELEVANT: yes/no
REASON: <brief explanation>

Be conservative - only say "yes" if the memory would actually be useful for the current task."#;

        let prompt = format!(
            "## Stored Memory\n{}\n\n## Current Context\n{}\n\nIs this memory relevant to the current context?",
            memory_content, current_context
        );

        let response = self.complete(system, &prompt).await?;

        // Parse response
        let mut is_relevant = false;
        for line in response.lines() {
            let line = line.trim();
            if line.len() >= 9 && line[..9].eq_ignore_ascii_case("relevant:") {
                let value = line[9..].trim();
                is_relevant = value.eq_ignore_ascii_case("yes") || value.starts_with("yes");
                break;
            }
        }
        let reason = response
            .lines()
            .find(|line| line.to_lowercase().starts_with("reason:"))
            .map(|line| line.trim_start_matches(|c: char| !c.is_alphabetic()).trim())
            .unwrap_or(&response)
            .to_string();

        Ok((is_relevant, reason))
    }

    /// Check if new information contradicts existing information
    /// Returns true if the two statements are contradictory
    pub async fn check_contradiction(
        &self,
        new_content: &str,
        existing_content: &str,
    ) -> Result<bool> {
        let system = "You are a contradiction detector. Given two statements, determine if the new information directly contradicts the existing information. Reply with exactly YES or NO.";

        let prompt = format!(
            "## Existing Information\n{}\n\n## New Information\n{}\n\nDoes the new information contradict the existing information?",
            existing_content, new_content
        );

        let response = self.complete(system, &prompt).await?;
        let trimmed = response.trim().to_uppercase();
        Ok(trimmed.starts_with("YES"))
    }

    /// Extract memories from a session transcript
    pub async fn extract_memories(&self, transcript: &str) -> Result<Vec<ExtractedMemory>> {
        self.extract_memories_with_existing(transcript, &[]).await
    }

    /// Extract memories from a session transcript, aware of what's already stored.
    pub async fn extract_memories_with_existing(
        &self,
        transcript: &str,
        existing: &[String],
    ) -> Result<Vec<ExtractedMemory>> {
        let mut system = String::from(
            r#"You are a memory extraction assistant. Extract important NEW learnings from the conversation that should be remembered for future sessions.

Categories (use EXACTLY one of these):
- fact: Technical facts about the codebase, architecture, patterns, dependencies, tools, environment
- preference: User preferences, workflow habits, UX expectations, coding style, conventions, how they want the assistant to behave
- correction: Mistakes that were corrected, bugs found and fixed, wrong assumptions, things the user corrected
- entity: Named entities worth tracking - people, projects, services, repos, teams

Categorization rules:
- If it describes what the USER WANTS or HOW THEY LIKE THINGS, it is "preference", not "fact"
- If it describes a BUG FIX or MISTAKE, it is "correction", not "fact"
- "fact" is for objective technical information about code/systems, not user behavior

IMPORTANT - Do NOT extract:
- Transient debugging details, compile errors, or intermediate build steps
- Specific commit hashes, git operations, or "changes were committed/pushed" details
- Line-by-line code changes like "X was updated to Y in file Z" - these belong in git history, not memory
- Self-evident project context (e.g., the project name, repo URL, language) that is already in the system prompt
- Redundant variations of information already known (check the "Already known" list carefully)

Quality bar: Only extract information that would ACTUALLY BE USEFUL if recalled in a future session on a different topic. Ask: "Would a developer benefit from knowing this weeks from now?"

For each memory, output in this format (one per line):
CATEGORY|CONTENT|TRUST

Where:
- CATEGORY is one of: fact, preference, correction, entity
- CONTENT is a concise statement (1-2 sentences max, under 200 characters preferred)
- TRUST is one of: high (user stated), medium (observed), low (inferred)

Output ONLY the formatted lines, no other text. If no NEW memories worth extracting, output nothing."#,
        );

        if !existing.is_empty() {
            system.push_str("\n\nAlready known (do NOT re-extract these or close paraphrases):\n");
            for mem in existing.iter().take(80) {
                system.push_str("- ");
                system.push_str(crate::util::truncate_str(mem, 150));
                system.push('\n');
            }
        }

        let response = self.complete(&system, transcript).await?;

        let memories = response
            .lines()
            .filter(|line| line.contains('|'))
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() >= 3 {
                    Some(ExtractedMemory {
                        category: parts[0].trim().to_lowercase(),
                        content: parts[1].trim().to_string(),
                        trust: parts[2].trim().to_lowercase(),
                    })
                } else {
                    None
                }
            })
            .collect();

        Ok(memories)
    }
}

impl Default for Sidecar {
    fn default() -> Self {
        Self::new()
    }
}

/// The public model constant for backward compatibility in tests.
#[cfg(test)]
pub const SIDECAR_FAST_MODEL: &str = SIDECAR_OPENAI_MODEL;

fn cloud_sidecar_fallback() -> (SidecarBackend, String, Option<LocalOpenAiEndpoint>) {
    if auth::codex::load_credentials().is_ok() {
        (
            SidecarBackend::OpenAI,
            SIDECAR_OPENAI_MODEL.to_string(),
            None,
        )
    } else {
        (
            SidecarBackend::Claude,
            SIDECAR_CLAUDE_MODEL.to_string(),
            None,
        )
    }
}

fn known_cloud_sidecar_backend(model: &str) -> Option<SidecarBackend> {
    if model == SIDECAR_OPENAI_MODEL || crate::provider::ALL_OPENAI_MODELS.contains(&model) {
        Some(SidecarBackend::OpenAI)
    } else if model == SIDECAR_CLAUDE_MODEL || crate::provider::ALL_CLAUDE_MODELS.contains(&model) {
        Some(SidecarBackend::Claude)
    } else {
        None
    }
}

fn heuristic_cloud_sidecar_backend(model: &str) -> Option<SidecarBackend> {
    match crate::provider::provider_for_model(model) {
        Some("openai") => Some(SidecarBackend::OpenAI),
        Some("claude") => Some(SidecarBackend::Claude),
        _ => None,
    }
}

fn resolve_local_openai_sidecar(
    configured_model: Option<&str>,
    cfg: &crate::config::Config,
) -> Option<(String, LocalOpenAiEndpoint)> {
    let candidate = local_openai_candidate(cfg)?;
    let model = configured_model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            cfg.provider
                .default_model
                .as_deref()
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(ToString::to_string)
        })
        .or(candidate.default_model)
        .or_else(|| candidate.static_models.into_iter().next())?;

    Some((model, candidate.endpoint))
}

fn local_openai_candidate(cfg: &crate::config::Config) -> Option<LocalOpenAiCandidate> {
    active_local_openai_candidate(cfg).or_else(|| default_provider_local_openai_candidate(cfg))
}

fn active_local_openai_candidate(cfg: &crate::config::Config) -> Option<LocalOpenAiCandidate> {
    for key in [
        "JCODE_NAMED_PROVIDER_PROFILE",
        "JCODE_PROVIDER_PROFILE_NAME",
    ] {
        if let Ok(profile_name) = std::env::var(key) {
            let profile_name = profile_name.trim();
            if let Some(profile) = cfg.providers.get(profile_name)
                && let Some(candidate) = local_openai_candidate_from_named_profile(profile)
            {
                return Some(candidate);
            }
        }
    }

    if let Ok(namespace) = std::env::var("JCODE_OPENROUTER_CACHE_NAMESPACE") {
        let namespace = namespace.trim();
        if let Some(profile) = cfg.providers.get(namespace)
            && let Some(candidate) = local_openai_candidate_from_named_profile(profile)
        {
            return Some(candidate);
        }
        if let Some(profile) = crate::provider_catalog::openai_compatible_profile_by_id(namespace)
            && let Some(candidate) = local_openai_candidate_from_profile(profile)
        {
            return Some(candidate);
        }
    }

    local_openai_candidate_from_openrouter_env()
}

fn default_provider_local_openai_candidate(
    cfg: &crate::config::Config,
) -> Option<LocalOpenAiCandidate> {
    let provider = cfg
        .provider
        .default_provider
        .as_deref()
        .map(str::trim)
        .filter(|provider| !provider.is_empty())?;

    if let Some(profile) =
        crate::provider_catalog::resolve_openai_compatible_profile_selection(provider)
    {
        return local_openai_candidate_from_profile(profile);
    }

    cfg.providers
        .get(provider)
        .and_then(local_openai_candidate_from_named_profile)
}

fn local_openai_candidate_from_profile(
    profile: crate::provider_catalog::OpenAiCompatibleProfile,
) -> Option<LocalOpenAiCandidate> {
    let resolved = crate::provider_catalog::resolve_openai_compatible_profile(profile);
    if !api_base_is_local(&resolved.api_base) {
        return None;
    }

    let auth = crate::provider_catalog::load_api_key_from_env_or_config(
        &resolved.api_key_env,
        &resolved.env_file,
    )
    .map(LocalOpenAiAuth::Bearer)
    .or_else(|| (!resolved.requires_api_key).then_some(LocalOpenAiAuth::None))?;

    Some(LocalOpenAiCandidate {
        endpoint: LocalOpenAiEndpoint {
            base_url: resolved.api_base,
            auth,
        },
        default_model: resolved
            .default_model
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty()),
        static_models: crate::provider_catalog::openai_compatible_profile_static_models(profile),
    })
}

fn local_openai_candidate_from_named_profile(
    profile: &crate::config::NamedProviderConfig,
) -> Option<LocalOpenAiCandidate> {
    let base_url = crate::provider_catalog::normalize_api_base(&profile.base_url)?;
    if !api_base_is_local(&base_url) {
        return None;
    }

    let auth = local_auth_from_named_profile(profile, &base_url)?;
    Some(LocalOpenAiCandidate {
        endpoint: LocalOpenAiEndpoint { base_url, auth },
        default_model: profile
            .default_model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(ToString::to_string),
        static_models: profile
            .models
            .iter()
            .map(|model| model.id.trim())
            .filter(|model| !model.is_empty())
            .map(ToString::to_string)
            .collect(),
    })
}

fn local_auth_from_named_profile(
    profile: &crate::config::NamedProviderConfig,
    base_url: &str,
) -> Option<LocalOpenAiAuth> {
    let key = profile
        .api_key_env
        .as_deref()
        .map(str::trim)
        .filter(|env| !env.is_empty())
        .and_then(|env| {
            if let Some(env_file) = profile
                .env_file
                .as_deref()
                .map(str::trim)
                .filter(|file| !file.is_empty())
            {
                crate::provider_catalog::load_api_key_from_env_or_config(env, env_file)
            } else {
                std::env::var(env)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            }
        })
        .or_else(|| {
            profile
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(ToString::to_string)
        });

    let requires_key = profile
        .requires_api_key
        .unwrap_or(!api_base_is_local(base_url));
    match profile.auth {
        crate::config::NamedProviderAuth::None => Some(LocalOpenAiAuth::None),
        crate::config::NamedProviderAuth::Bearer => key
            .map(LocalOpenAiAuth::Bearer)
            .or_else(|| (!requires_key).then_some(LocalOpenAiAuth::None)),
        crate::config::NamedProviderAuth::Header => key
            .map(|value| LocalOpenAiAuth::Header {
                name: profile
                    .auth_header
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or("api-key")
                    .to_string(),
                value,
            })
            .or_else(|| (!requires_key).then_some(LocalOpenAiAuth::None)),
    }
}

fn local_openai_candidate_from_openrouter_env() -> Option<LocalOpenAiCandidate> {
    let base_url = std::env::var("JCODE_OPENROUTER_API_BASE")
        .ok()
        .and_then(|value| crate::provider_catalog::normalize_api_base(value.trim()))?;
    if !api_base_is_local(&base_url) {
        return None;
    }

    let key_name = std::env::var("JCODE_OPENROUTER_API_KEY_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "OPENROUTER_API_KEY".to_string());
    let env_file = std::env::var("JCODE_OPENROUTER_ENV_FILE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "openrouter.env".to_string());
    let key = crate::provider_catalog::load_api_key_from_env_or_config(&key_name, &env_file);
    let allow_no_auth = std::env::var("JCODE_OPENROUTER_ALLOW_NO_AUTH")
        .ok()
        .is_some_and(|value| parse_bool_like(&value));

    let auth = match key {
        Some(value) if openrouter_env_uses_api_key_header() => LocalOpenAiAuth::Header {
            name: std::env::var("JCODE_OPENROUTER_AUTH_HEADER_NAME")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "api-key".to_string()),
            value,
        },
        Some(value) => LocalOpenAiAuth::Bearer(value),
        None if allow_no_auth || api_base_is_local(&base_url) => LocalOpenAiAuth::None,
        None => return None,
    };

    Some(LocalOpenAiCandidate {
        endpoint: LocalOpenAiEndpoint { base_url, auth },
        default_model: std::env::var("JCODE_OPENROUTER_MODEL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        static_models: std::env::var("JCODE_OPENROUTER_STATIC_MODELS")
            .ok()
            .map(|value| crate::provider_catalog::parse_openai_compatible_models(&value))
            .unwrap_or_default(),
    })
}

fn openrouter_env_uses_api_key_header() -> bool {
    std::env::var("JCODE_OPENROUTER_AUTH_HEADER")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "api-key" | "apikey" | "header"
            )
        })
}

fn parse_bool_like(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn api_base_is_local(raw: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw) else {
        return false;
    };
    let Some(host) = parsed.host_str().map(|host| host.to_ascii_lowercase()) else {
        return false;
    };

    if matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") || host.ends_with(".local") {
        return true;
    }

    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(addr)) => addr.is_loopback() || addr.is_private() || addr.is_link_local(),
        Ok(IpAddr::V6(addr)) => {
            let first = addr.segments()[0];
            addr.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

fn resolve_openai_request_model(
    preferred_model: &str,
    is_chatgpt_mode: bool,
) -> (&str, Option<&'static str>) {
    if !is_chatgpt_mode || preferred_model != SIDECAR_OPENAI_MODEL {
        return (preferred_model, None);
    }

    match crate::provider::is_model_available_for_account(SIDECAR_OPENAI_MODEL) {
        Some(false) => (
            SIDECAR_OPENAI_OAUTH_FALLBACK_MODEL,
            Some(SIDECAR_OPENAI_OAUTH_FALLBACK_REASONING),
        ),
        _ => (SIDECAR_OPENAI_MODEL, None),
    }
}

fn build_openai_request(
    model: &str,
    system: &str,
    user_message: &str,
    stream: bool,
    reasoning_effort: Option<&str>,
) -> serde_json::Value {
    let mut instructions = String::new();
    if !system.is_empty() {
        instructions.push_str(system);
    }

    let mut request = serde_json::json!({
        "model": model,
        "instructions": instructions,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": user_message,
            }],
        }],
        "stream": stream,
        "store": false,
    });

    if let Some(effort) = reasoning_effort {
        request["reasoning"] = serde_json::json!({ "effort": effort });
    }

    request
}

fn build_local_chat_request(
    model: &str,
    system: &str,
    user_message: &str,
    max_tokens: u32,
) -> serde_json::Value {
    let mut messages = Vec::new();
    if !system.is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": system,
        }));
    }
    messages.push(serde_json::json!({
        "role": "user",
        "content": user_message,
    }));

    serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": false,
        "max_tokens": max_tokens,
    })
}

fn extract_local_chat_response_text(result: &serde_json::Value) -> Result<String> {
    let choices = result
        .get("choices")
        .and_then(|value| value.as_array())
        .context("Local OpenAI-compatible response missing choices")?;

    for choice in choices {
        if let Some(content) = choice
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(local_chat_content_to_text)
        {
            return Ok(content);
        }
    }

    Ok(String::new())
}

fn local_chat_content_to_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }

    let parts = value.as_array()?;
    let text = parts
        .iter()
        .filter_map(|part| {
            part.as_str().map(ToString::to_string).or_else(|| {
                part.get("text")
                    .and_then(|value| value.as_str())
                    .map(ToString::to_string)
            })
        })
        .collect::<Vec<_>>()
        .join("");
    Some(text)
}

fn classify_openai_model_unavailable(status: StatusCode, body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    let mentions_model = lower.contains("model")
        || lower.contains("slug")
        || lower.contains("engine")
        || lower.contains("deployment");
    let unavailable = lower.contains("not available")
        || lower.contains("unavailable")
        || lower.contains("does not have access")
        || lower.contains("not enabled")
        || lower.contains("not found")
        || lower.contains("unknown model")
        || lower.contains("unsupported model")
        || lower.contains("invalid model");

    if !mentions_model || !unavailable {
        return None;
    }

    if matches!(
        status,
        StatusCode::NOT_FOUND
            | StatusCode::FORBIDDEN
            | StatusCode::BAD_REQUEST
            | StatusCode::UNPROCESSABLE_ENTITY
    ) {
        let trimmed = body.trim();
        return Some(if trimmed.is_empty() {
            format!("model denied by OpenAI API (status {})", status)
        } else {
            format!(
                "model denied by OpenAI API (status {}): {}",
                status, trimmed
            )
        });
    }

    None
}

fn is_openai_model_unavailable(status: StatusCode, body: &str) -> bool {
    classify_openai_model_unavailable(status, body).is_some()
}

enum OpenAiSidecarError {
    Api { status: StatusCode, body: String },
    Other(anyhow::Error),
}

impl OpenAiSidecarError {
    fn other(err: anyhow::Error) -> Self {
        Self::Other(err)
    }

    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Api { status, body } => {
                anyhow::anyhow!("OpenAI API error ({}): {}", status, body)
            }
            Self::Other(err) => err,
        }
    }
}

/// A memory extracted by the sidecar
#[derive(Debug, Clone)]
pub struct ExtractedMemory {
    pub category: String,
    pub content: String,
    pub trust: String,
}

/// Collect text from an OpenAI Responses API SSE stream.
///
/// Parses `data: <json>` lines and accumulates text deltas from
/// `response.output_text.delta` events, stopping on completion/done.
async fn collect_openai_sse_text(response: reqwest::Response) -> Result<String> {
    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    let mut text = String::new();
    let mut buf = String::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("Error reading SSE stream")?;
        buf.push_str(&String::from_utf8_lossy(&bytes));

        // Process all complete lines in the buffer
        while let Some(newline_pos) = buf.find('\n') {
            let line = buf[..newline_pos].trim_end_matches('\r').to_string();
            buf = buf[newline_pos + 1..].to_string();

            if let Some(data) = crate::util::sse_data_line(&line) {
                if data == "[DONE]" {
                    return Ok(text);
                }
                if let Ok(event) = serde_json::from_str::<SseEvent>(data) {
                    match event.kind.as_str() {
                        "response.output_text.delta" => {
                            if let Some(delta) = event.delta {
                                text.push_str(&delta);
                            }
                        }
                        "response.completed" | "response.incomplete" => {
                            return Ok(text);
                        }
                        "response.failed" | "error" => {
                            let msg = event
                                .error
                                .as_ref()
                                .and_then(|e| e.as_str())
                                .unwrap_or("unknown error");
                            anyhow::bail!("OpenAI SSE error: {}", msg);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    Ok(text)
}

/// Extract text from a non-streaming OpenAI Responses API JSON response.
fn extract_openai_response_text(result: &serde_json::Value) -> Result<String> {
    let mut text = String::new();
    if let Some(output) = result.get("output").and_then(|v| v.as_array()) {
        for item in output {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if item_type == "message"
                && let Some(content) = item.get("content").and_then(|v| v.as_array())
            {
                for block in content {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if (block_type == "output_text" || block_type == "text")
                        && let Some(t) = block.get("text").and_then(|v| v.as_str())
                    {
                        text.push_str(t);
                    }
                }
            }
        }
    }
    Ok(text)
}

#[derive(Deserialize)]
struct SseEvent {
    #[serde(rename = "type")]
    kind: String,
    delta: Option<String>,
    error: Option<serde_json::Value>,
}

// Claude API types

#[derive(Serialize)]
struct ClaudeMessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<ClaudeApiSystem<'a>>,
    messages: Vec<ClaudeMessage<'a>>,
}

#[derive(Serialize)]
struct ClaudeMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ClaudeApiSystem<'a> {
    Blocks(Vec<ClaudeApiSystemBlock<'a>>),
}

#[derive(Serialize)]
struct ClaudeApiSystemBlock<'a> {
    #[serde(rename = "type")]
    block_type: &'static str,
    text: &'a str,
}

fn build_claude_system_param(system: &str) -> Option<ClaudeApiSystem<'_>> {
    let mut blocks = Vec::new();
    blocks.push(ClaudeApiSystemBlock {
        block_type: "text",
        text: CLAUDE_CODE_IDENTITY,
    });
    blocks.push(ClaudeApiSystemBlock {
        block_type: "text",
        text: CLAUDE_CODE_JCODE_NOTICE,
    });
    if !system.is_empty() {
        blocks.push(ClaudeApiSystemBlock {
            block_type: "text",
            text: system,
        });
    }
    Some(ClaudeApiSystem::Blocks(blocks))
}

#[derive(Deserialize)]
struct ClaudeMessagesResponse {
    content: Vec<ClaudeContentBlock>,
    #[serde(rename = "usage")]
    _usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClaudeContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    #[serde(rename = "input_tokens")]
    _input_tokens: u32,
    #[serde(rename = "output_tokens")]
    _output_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::codex;
    use std::ffi::{OsStr, OsString};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set<K: AsRef<OsStr>>(key: &'static str, value: K) -> Self {
            let previous = std::env::var_os(key);
            crate::env::set_var(key, value);
            Self { key, previous }
        }

        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(key);
            crate::env::set_var(key, value);
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            crate::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                crate::env::set_var(self.key, previous);
            } else {
                crate::env::remove_var(self.key);
            }
        }
    }

    fn unset_vars(keys: &[&'static str]) -> Vec<EnvVarGuard> {
        keys.iter().copied().map(EnvVarGuard::unset).collect()
    }

    fn local_test_env_vars() -> Vec<EnvVarGuard> {
        unset_vars(&[
            "JCODE_NAMED_PROVIDER_PROFILE",
            "JCODE_PROVIDER_PROFILE_NAME",
            "JCODE_OPENROUTER_CACHE_NAMESPACE",
            "JCODE_OPENROUTER_API_BASE",
            "JCODE_OPENROUTER_API_KEY_NAME",
            "JCODE_OPENROUTER_ENV_FILE",
            "JCODE_OPENROUTER_ALLOW_NO_AUTH",
            "JCODE_OPENROUTER_MODEL",
            "JCODE_OPENROUTER_STATIC_MODELS",
            "JCODE_OPENROUTER_AUTH_HEADER",
            "JCODE_OPENROUTER_AUTH_HEADER_NAME",
            "OPENROUTER_API_KEY",
            "OPENAI_API_KEY",
        ])
    }

    fn local_named_config(base_url: &str) -> crate::config::Config {
        let mut cfg = crate::config::Config::default();
        cfg.provider.default_provider = Some("local-memory".to_string());
        cfg.provider.default_model = Some("provider-default".to_string());
        cfg.providers.insert(
            "local-memory".to_string(),
            crate::config::NamedProviderConfig {
                base_url: base_url.to_string(),
                auth: crate::config::NamedProviderAuth::None,
                default_model: Some("profile-default".to_string()),
                models: vec![crate::config::NamedProviderModelConfig {
                    id: "static-default".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        cfg
    }

    #[test]
    fn test_sidecar_fast_model() {
        assert_eq!(SIDECAR_FAST_MODEL, "gpt-5.3-codex-spark");
    }

    #[test]
    fn test_backend_selection_prefers_openai() {
        // Make backend selection deterministic by isolating credentials.
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("create temp jcode home");
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
        let _openai = EnvVarGuard::unset("OPENAI_API_KEY");

        codex::upsert_account_from_tokens("openai-1", "sk-test-key-123", "", None, None)
            .expect("write OpenAI test auth");
        crate::auth::claude::upsert_account(crate::auth::claude::AnthropicAccount {
            label: "claude-1".to_string(),
            access: "claude-access".to_string(),
            refresh: "claude-refresh".to_string(),
            expires: 4_102_444_800_000,
            email: None,
            scopes: Vec::new(),
            subscription_type: None,
        })
        .expect("write Claude test auth");

        let sidecar = Sidecar::with_configured_model(None);
        assert_eq!(sidecar.backend, SidecarBackend::OpenAI);
        assert_eq!(sidecar.model, SIDECAR_OPENAI_MODEL);
        codex::set_active_account_override(None);
        crate::auth::claude::set_active_account_override(None);
    }

    #[test]
    fn test_backend_selection_prefers_local_named_provider_over_openai_creds() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("create temp jcode home");
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
        let _env = local_test_env_vars();

        codex::upsert_account_from_tokens("openai-1", "sk-test-key-123", "", None, None)
            .expect("write OpenAI test auth");

        let cfg = local_named_config("http://localhost:11434/v1");
        let sidecar = Sidecar::with_configured_model_and_config(None, &cfg);

        assert_eq!(sidecar.backend, SidecarBackend::LocalOpenAI);
        assert_eq!(sidecar.backend_name(), "local");
        assert_eq!(sidecar.model, "provider-default");
        assert_eq!(
            sidecar
                .local_openai
                .as_ref()
                .map(|endpoint| endpoint.base_url.as_str()),
            Some("http://localhost:11434/v1")
        );
        codex::set_active_account_override(None);
    }

    #[test]
    fn test_memory_model_override_uses_local_default_provider() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("create temp jcode home");
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
        let _env = local_test_env_vars();

        let cfg = local_named_config("http://127.0.0.1:11434/v1");
        let sidecar = Sidecar::with_configured_model_and_config(Some("llama3.2".to_string()), &cfg);

        assert_eq!(sidecar.backend, SidecarBackend::LocalOpenAI);
        assert_eq!(sidecar.model, "llama3.2");
    }

    #[test]
    fn test_gpt_like_memory_model_override_stays_local_when_local_provider_is_configured() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("create temp jcode home");
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
        let _env = local_test_env_vars();

        let cfg = local_named_config("http://127.0.0.1:11434/v1");
        let sidecar =
            Sidecar::with_configured_model_and_config(Some("gpt-oss-120b".to_string()), &cfg);

        assert_eq!(sidecar.backend, SidecarBackend::LocalOpenAI);
        assert_eq!(sidecar.model, "gpt-oss-120b");
    }

    #[test]
    fn test_remote_openai_compatible_default_does_not_auto_route_memory() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("create temp jcode home");
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
        let _env = local_test_env_vars();

        let mut cfg = crate::config::Config::default();
        cfg.provider.default_provider = Some("remote-memory".to_string());
        cfg.provider.default_model = Some("remote-model".to_string());
        cfg.providers.insert(
            "remote-memory".to_string(),
            crate::config::NamedProviderConfig {
                base_url: "https://compat.example.test/v1".to_string(),
                auth: crate::config::NamedProviderAuth::None,
                default_model: Some("remote-model".to_string()),
                ..Default::default()
            },
        );

        let sidecar = Sidecar::with_configured_model_and_config(None, &cfg);

        assert_eq!(sidecar.backend, SidecarBackend::Claude);
        assert_eq!(sidecar.model, SIDECAR_CLAUDE_MODEL);
        assert!(sidecar.local_openai.is_none());
    }

    #[test]
    fn test_explicit_openai_and_claude_memory_overrides_are_unchanged() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("create temp jcode home");
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
        let _env = local_test_env_vars();
        let cfg = local_named_config("http://localhost:11434/v1");

        let openai =
            Sidecar::with_configured_model_and_config(Some(SIDECAR_OPENAI_MODEL.to_string()), &cfg);
        assert_eq!(openai.backend, SidecarBackend::OpenAI);
        assert_eq!(openai.model, SIDECAR_OPENAI_MODEL);

        let claude =
            Sidecar::with_configured_model_and_config(Some(SIDECAR_CLAUDE_MODEL.to_string()), &cfg);
        assert_eq!(claude.backend, SidecarBackend::Claude);
        assert_eq!(claude.model, SIDECAR_CLAUDE_MODEL);
    }

    #[tokio::test]
    async fn test_local_openai_complete_uses_chat_completions_endpoint() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("create temp jcode home");
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
        let mut env = local_test_env_vars();
        env.push(EnvVarGuard::set("NO_PROXY", "127.0.0.1,localhost"));
        env.push(EnvVarGuard::set("no_proxy", "127.0.0.1,localhost"));

        let (base_url, request_rx, handle) = spawn_local_chat_server();
        let mut cfg = crate::config::Config::default();
        cfg.provider.default_provider = Some("local-memory".to_string());
        cfg.providers.insert(
            "local-memory".to_string(),
            crate::config::NamedProviderConfig {
                base_url,
                auth: crate::config::NamedProviderAuth::Header,
                auth_header: Some("x-api-key".to_string()),
                api_key: Some("secret".to_string()),
                default_model: Some("local-chat".to_string()),
                ..Default::default()
            },
        );

        let sidecar = Sidecar::with_configured_model_and_config(None, &cfg);
        let text = sidecar
            .complete("system prompt", "user prompt")
            .await
            .expect("local sidecar response");
        assert_eq!(text, "local ok");

        let request = request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("server captured request");
        assert!(request.starts_with("POST /v1/chat/completions "));
        assert!(request.to_ascii_lowercase().contains("x-api-key: secret"));
        assert!(request.contains(r#""model":"local-chat""#));
        assert!(request.contains(r#""role":"system""#));
        assert!(request.contains(r#""content":"system prompt""#));
        assert!(request.contains(r#""role":"user""#));
        assert!(request.contains(r#""content":"user prompt""#));
        handle.join().expect("server thread");
    }

    fn spawn_local_chat_server() -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local chat server");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept local chat request");
            let request = read_http_request(&mut stream);
            tx.send(request).expect("send captured request");
            let body = r#"{"choices":[{"message":{"content":"local ok"}}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write response");
        });

        (format!("http://{}/v1", addr), rx, handle)
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buf = [0_u8; 1024];
        loop {
            let n = stream.read(&mut buf).expect("read request");
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
            let request = String::from_utf8_lossy(&bytes);
            if let Some(header_end) = request.find("\r\n\r\n") {
                let content_len = content_length(&request[..header_end]).unwrap_or(0);
                if bytes.len() >= header_end + 4 + content_len {
                    break;
                }
            }
        }
        String::from_utf8(bytes).expect("utf8 request")
    }

    fn content_length(headers: &str) -> Option<usize> {
        headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
    }

    #[test]
    fn test_chatgpt_oauth_keeps_spark_when_available() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("create temp jcode home");
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
        codex::set_active_account_override(Some("openai-1".to_string()));
        crate::provider::clear_all_model_unavailability_for_account();
        crate::provider::populate_account_models(vec![
            SIDECAR_OPENAI_MODEL.to_string(),
            SIDECAR_OPENAI_OAUTH_FALLBACK_MODEL.to_string(),
        ]);

        let (model, reasoning) = resolve_openai_request_model(SIDECAR_OPENAI_MODEL, true);
        assert_eq!(model, SIDECAR_OPENAI_MODEL);
        assert_eq!(reasoning, None);

        codex::set_active_account_override(None);
    }

    #[test]
    fn test_chatgpt_oauth_falls_back_to_gpt_5_4_low_when_spark_unavailable() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("create temp jcode home");
        let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
        codex::set_active_account_override(Some("openai-1".to_string()));
        crate::provider::clear_all_model_unavailability_for_account();
        crate::provider::populate_account_models(vec![
            SIDECAR_OPENAI_OAUTH_FALLBACK_MODEL.to_string(),
        ]);

        let (model, reasoning) = resolve_openai_request_model(SIDECAR_OPENAI_MODEL, true);
        assert_eq!(model, SIDECAR_OPENAI_OAUTH_FALLBACK_MODEL);
        assert_eq!(reasoning, Some(SIDECAR_OPENAI_OAUTH_FALLBACK_REASONING));

        codex::set_active_account_override(None);
    }

    #[test]
    fn test_build_openai_request_adds_low_reasoning_only_for_fallback() {
        let request = build_openai_request(
            SIDECAR_OPENAI_OAUTH_FALLBACK_MODEL,
            "system",
            "hello",
            true,
            Some(SIDECAR_OPENAI_OAUTH_FALLBACK_REASONING),
        );
        assert_eq!(request["model"], SIDECAR_OPENAI_OAUTH_FALLBACK_MODEL);
        assert_eq!(
            request["reasoning"],
            serde_json::json!({"effort": SIDECAR_OPENAI_OAUTH_FALLBACK_REASONING})
        );

        let spark_request =
            build_openai_request(SIDECAR_OPENAI_MODEL, "system", "hello", true, None);
        assert!(spark_request.get("reasoning").is_none());
    }
}
