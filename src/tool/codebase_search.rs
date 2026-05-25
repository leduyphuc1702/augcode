use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use jcode_codebase_retrieval::{
    CodebaseRetrievalEngine, RetrievalRequest, RetrievalUsageTrace, UnsavedBuffer,
    record_retrieval_usage_trace, root_from_context_path,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct CodebaseSearchTool;

impl CodebaseSearchTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct CodebaseSearchInput {
    query: String,
    #[serde(default)]
    active_file: Option<String>,
    #[serde(default)]
    token_budget: Option<usize>,
    #[serde(default)]
    unsaved_buffers: Vec<UnsavedBuffer>,
    #[serde(default)]
    include_trace: bool,
}

#[async_trait]
impl Tool for CodebaseSearchTool {
    fn name(&self) -> &str {
        "codebase_search"
    }

    fn description(&self) -> &str {
        "Search the current workspace using JCode's local manifest and overlay index."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "intent": super::intent_schema_property(),
                "query": {
                    "type": "string",
                    "description": "Natural-language or lexical query for current workspace code."
                },
                "active_file": {
                    "type": "string",
                    "description": "Optional active file path used for future ranking."
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Approximate output token budget."
                },
                "include_trace": {
                    "type": "boolean",
                    "description": "Include retrieval ranking and token trace in the output."
                },
                "unsaved_buffers": {
                    "type": "array",
                    "description": "Optional editor buffers that have not been saved yet.",
                    "items": {
                        "type": "object",
                        "required": ["path", "contents"],
                        "properties": {
                            "path": { "type": "string" },
                            "contents": { "type": "string" }
                        }
                    }
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: CodebaseSearchInput = serde_json::from_value(input)?;
        let root = root_from_context_path(ctx.working_dir)?;
        let engine = CodebaseRetrievalEngine::default_engine()?;
        let response = engine.search(
            &root,
            RetrievalRequest {
                query: params.query.clone(),
                active_file: params.active_file,
                token_budget: params.token_budget,
                unsaved_buffers: params.unsaved_buffers,
                include_trace: params.include_trace,
            },
        )?;
        let _ = record_retrieval_usage_trace(
            &root,
            &RetrievalUsageTrace {
                timestamp: chrono::Utc::now().to_rfc3339(),
                session_id: ctx.session_id,
                message_id: ctx.message_id,
                tool_call_id: ctx.tool_call_id,
                tool_name: "codebase_search".to_string(),
                event_kind: "retrieval_context".to_string(),
                action_kind: Some("retrieval_context".to_string()),
                command_label: None,
                paths: response
                    .context_pack
                    .files
                    .iter()
                    .map(|file| file.path.clone())
                    .collect(),
                context_token_estimate: response
                    .context_pack
                    .files
                    .iter()
                    .map(|file| file.token_estimate)
                    .sum(),
                context_used_token_estimate: 0,
                context_waste_after_turn_bps: 0,
                edit_hit_rate_bps: 0,
                test_hit_rate_bps: 0,
                retrieval_to_edit_distance: None,
            },
        );
        Ok(ToolOutput::new(render_response(&response))
            .with_title(format!("codebase_search: {}", params.query)))
    }
}

fn render_response(response: &jcode_codebase_retrieval::SearchResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("snapshot: {}\n", response.snapshot_id));
    out.push_str(&format!(
        "freshness: local_overlay={} unsaved_buffers={}\n",
        response.freshness.local_overlay_included, response.freshness.unsaved_buffers_included
    ));
    if response.context_pack.files.is_empty() {
        out.push_str("No local context found.\n");
        return out;
    }
    for file in &response.context_pack.files {
        out.push_str(&format!(
            "\n{} [{}]\nwhy: {}\n",
            file.path, file.content_hash, file.why_included
        ));
        for range in &file.ranges {
            out.push_str(&format!(
                "lines {}-{}:\n{}\n",
                range.start_line, range.end_line, range.text
            ));
        }
    }
    for omitted in &response.context_pack.omitted {
        out.push_str(&format!(
            "\nomitted: {} ({})\n",
            omitted.count, omitted.reason
        ));
    }
    if let Some(trace) = &response.trace {
        out.push_str(&format!(
            "\ntrace: candidates={} returned={} omitted={} token_used={} budget={}\n",
            trace.candidate_count,
            trace.returned_count,
            trace.omitted_count,
            trace.token_used,
            trace.token_budget
        ));
        for candidate in trace.candidates.iter().take(40) {
            out.push_str(&format!(
                "- {} [{}] score={}/{} tokens={} lines {}-{} kind={} edge={} reason={}{}{}\n",
                candidate.path,
                candidate.source,
                candidate.raw_score,
                candidate.final_score,
                candidate.token_estimate,
                candidate.start_line,
                candidate.end_line,
                candidate.node_kind.as_deref().unwrap_or("file"),
                candidate.edge_kind.as_deref().unwrap_or(""),
                candidate.reason,
                candidate
                    .omitted_reason
                    .as_ref()
                    .map(|reason| format!(" omitted={reason}"))
                    .unwrap_or_default(),
                if candidate.graph_path.is_empty() {
                    String::new()
                } else {
                    format!(" graph_path={}", candidate.graph_path.join(" -> "))
                }
            ));
        }
    }
    out
}

#[cfg(test)]
#[path = "codebase_search_tests.rs"]
mod tests;
