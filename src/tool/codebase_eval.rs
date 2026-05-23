use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use jcode_codebase_retrieval::{
    CodebaseRetrievalEngine, RetrievalEvalCase, root_from_context_path,
};

#[derive(Debug, Deserialize)]
struct FreshnessBenchmarkInput {
    path: String,
    contents: String,
    query: String,
}
use serde::Deserialize;
use serde_json::{Value, json};

pub struct CodebaseEvalTool;

impl CodebaseEvalTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct CodebaseEvalInput {
    #[serde(default)]
    cases: Vec<RetrievalEvalCase>,
    #[serde(default)]
    fixture_path: Option<String>,
    #[serde(default)]
    freshness_benchmark: Option<FreshnessBenchmarkInput>,
}

#[async_trait]
impl Tool for CodebaseEvalTool {
    fn name(&self) -> &str {
        "codebase_eval"
    }

    fn description(&self) -> &str {
        "Evaluate local codebase retrieval with deterministic recall and freshness metrics."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "cases": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["query", "expected_files"],
                        "properties": {
                            "query": { "type": "string" },
                            "expected_files": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "intent": { "type": "string" },
                            "active_file": { "type": "string" },
                            "must_not_return": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "category": { "type": "string" }
                        }
                    }
                },
                "fixture_path": {
                    "type": "string",
                    "description": "Optional JSON file path containing retrieval eval cases."
                },
                "freshness_benchmark": {
                    "type": "object",
                    "required": ["path", "contents", "query"],
                    "properties": {
                        "path": { "type": "string" },
                        "contents": { "type": "string" },
                        "query": { "type": "string" }
                    }
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: CodebaseEvalInput = serde_json::from_value(input)?;
        let root = root_from_context_path(ctx.working_dir)?;
        let engine = CodebaseRetrievalEngine::default_engine()?;
        let report = if let Some(fixture_path) = params.fixture_path {
            engine.eval_fixture(&root, std::path::Path::new(&fixture_path))?
        } else {
            engine.eval(&root, &params.cases)?
        };
        let mut out = String::new();
        out.push_str(&format!("cases_total: {}\n", report.cases_total));
        out.push_str(&format!("recall_at_5_hits: {}\n", report.recall_at_5_hits));
        out.push_str(&format!(
            "recall_at_5_rate_bps: {}\n",
            report.recall_at_5_rate_bps
        ));
        out.push_str(&format!(
            "stale_context_count: {}\n",
            report.stale_context_count
        ));
        out.push_str(&format!(
            "unauthorized_candidate_count: {}\n",
            report.unauthorized_candidate_count
        ));
        out.push_str(&format!(
            "forbidden_context_count: {}\n",
            report.forbidden_context_count
        ));
        out.push_str(&format!("mrr_bps: {}\n", report.mrr_bps));
        for category in &report.categories {
            out.push_str(&format!(
                "category:{} cases={} recall_at_5_bps={}\n",
                category.category, category.cases_total, category.recall_at_5_rate_bps
            ));
        }
        if !report.missing_expected.is_empty() {
            out.push_str("missing_expected:\n");
            for missing in &report.missing_expected {
                out.push_str(&format!(
                    "- query={} expected={:?} returned={:?}\n",
                    missing.query, missing.expected_files, missing.returned_files
                ));
            }
        }
        if let Some(bench) = params.freshness_benchmark {
            let latency = engine.benchmark_save_to_search(
                &root,
                &bench.path,
                &bench.contents,
                &bench.query,
            )?;
            out.push_str(&format!("freshness_path: {}\n", latency.path));
            out.push_str(&format!("freshness_query: {}\n", latency.query));
            out.push_str(&format!(
                "save_to_search_ms: {}\n",
                latency.save_to_search_ms
            ));
            out.push_str(&format!("freshness_found: {}\n", latency.found));
        }
        Ok(ToolOutput::new(out).with_title("codebase_eval"))
    }
}
