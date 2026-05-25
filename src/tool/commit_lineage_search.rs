use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use jcode_codebase_retrieval::root_from_context_path;
use jcode_codebase_sync::harvest_commit_lineage;
use serde::Deserialize;
use serde_json::{Value, json};

pub struct CommitLineageSearchTool;

impl CommitLineageSearchTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct CommitLineageSearchInput {
    query: String,
    #[serde(default)]
    #[serde(rename = "branch")]
    branch: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for CommitLineageSearchTool {
    fn name(&self) -> &str {
        "commit_lineage_search"
    }

    fn description(&self) -> &str {
        "Search recent commit history in the current workspace for patterns, \
         technical terms, or file changes relevant to a query."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "intent": super::intent_schema_property(),
                "query": {
                    "type": "string",
                    "description": "Search query for commit messages, changed files, or technical terms."
                },
                "branch": {
                    "type": "string",
                    "description": "Optional branch to search. Defaults to current branch."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum commits to return. Default 10."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: CommitLineageSearchInput = serde_json::from_value(input)?;
        let root = root_from_context_path(ctx.working_dir)?;
        let max_results = params.max_results.unwrap_or(10);
        let _branch = params.branch.as_deref();
        let docs = harvest_commit_lineage(&root, max_results)?;

        let query_lower = params.query.to_lowercase();
        let query_terms: Vec<&str> = query_lower
            .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
            .filter(|t| t.len() >= 2)
            .collect();

        let mut hits: Vec<(usize, jcode_codebase_sync::CommitSearchDocument)> = docs
            .into_iter()
            .map(|doc| {
                let mut score = 0usize;
                let msg_lower = doc.message.to_lowercase();
                for term in &query_terms {
                    if msg_lower.contains(term) {
                        score += 3;
                    }
                    for file in &doc.changed_files {
                        if file.to_lowercase().contains(term) {
                            score += 2;
                        }
                    }
                    for tech in &doc.technical_terms {
                        if tech.contains(term) {
                            score += 1;
                        }
                    }
                }
                (score, doc)
            })
            .filter(|(score, _)| *score > 0)
            .collect();

        hits.sort_by(|a, b| b.0.cmp(&a.0));
        hits.truncate(max_results);

        let mut out = String::new();
        if hits.is_empty() {
            out.push_str("No matching commits found.\n");
        } else {
            for (score, doc) in hits {
                out.push_str(&format!(
                    "\n{} [{}] (score={})\n{}
changed files: {:?}\nterms: {:?}\n",
                    doc.sha,
                    doc.timestamp.as_deref().unwrap_or("unknown"),
                    score,
                    doc.message,
                    doc.changed_files,
                    doc.technical_terms
                ));
            }
        }
        Ok(ToolOutput::new(out))
    }
}

#[cfg(test)]
#[path = "commit_lineage_search_tests.rs"]
mod tests;
