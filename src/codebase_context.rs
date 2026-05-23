use crate::message::{ContentBlock, Message, Role};
use jcode_codebase_retrieval::{CodebaseRetrievalEngine, RetrievalRequest};
use jcode_codebase_sync::{CodebaseSyncEngine, ManifestStore};
use std::path::PathBuf;

const CODEBASE_CONTEXT_TOKEN_BUDGET: usize = 4_000;

pub fn build_codebase_context_prompt(
    session_id: &str,
    working_dir: Option<&str>,
    messages: &[Message],
) -> Option<String> {
    if !crate::config::config().features.codebase_sync {
        return None;
    }
    let query = last_user_text(messages)?;
    if !is_code_related_query(&query) {
        return None;
    }
    let root = PathBuf::from(working_dir?);
    let sync = CodebaseSyncEngine::new(ManifestStore::default_store().ok()?);
    if sync.index_snapshot(&root).ok().flatten().is_none() {
        crate::codebase_sync_runtime::start_for_path(session_id.to_string(), &root);
        return None;
    }
    let response = CodebaseRetrievalEngine::new(sync)
        .search(
            &root,
            RetrievalRequest {
                query,
                active_file: None,
                token_budget: Some(CODEBASE_CONTEXT_TOKEN_BUDGET),
                unsaved_buffers: Vec::new(),
            },
        )
        .ok()?;
    if response.context_pack.files.is_empty() {
        return None;
    }
    Some(format_context_prompt(&response))
}

fn last_user_text(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        if message.role != Role::User {
            return None;
        }
        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        (!text.trim().is_empty()).then_some(text)
    })
}

fn is_code_related_query(query: &str) -> bool {
    let lower = query.to_lowercase();
    let keywords = [
        "code",
        "codebase",
        "file",
        "function",
        "fn ",
        "class",
        "struct",
        "impl",
        "test",
        "debug",
        "fix",
        "bug",
        "error",
        "panic",
        "crash",
        "refactor",
        "implement",
        "add ",
        "change",
        "modify",
        "update",
        "build",
        "cargo",
        "npm",
        "api",
        "route",
        "handler",
        "database",
        "schema",
        "migration",
        "config",
        "sửa",
        "lỗi",
        "hàm",
        "lớp",
        "triển khai",
        "kiểm tra",
        "xây dựng",
    ];
    keywords.iter().any(|keyword| lower.contains(keyword))
        || [
            ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java", ".toml", ".json",
        ]
        .iter()
        .any(|ext| lower.contains(ext))
}

fn format_context_prompt(response: &jcode_codebase_retrieval::SearchResponse) -> String {
    let mut out = String::new();
    out.push_str("# Codebase Context\n\n");
    out.push_str("Relevant current local index snippets for this turn. Use as hints; verify before editing.\n");
    out.push_str(&format!("Snapshot: `{}`\n\n", response.snapshot_id));
    for file in &response.context_pack.files {
        out.push_str(&format!("## `{}`\n", file.path));
        out.push_str(&format!("Reason: {}\n\n", file.why_included));
        for range in &file.ranges {
            out.push_str(&format!("Lines {}-{}:\n", range.start_line, range.end_line));
            out.push_str("```text\n");
            out.push_str(range.text.trim_end());
            out.push_str("\n```\n\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_code_related_queries() {
        assert!(is_code_related_query("fix auth panic"));
        assert!(is_code_related_query("hãy sửa lỗi trong file config"));
        assert!(!is_code_related_query("thời tiết hôm nay thế nào"));
    }
}
