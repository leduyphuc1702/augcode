use anyhow::{Context, Result};
use jcode_codebase_sync::{
    CodebaseSyncEngine, DependencyGraph, ExactVectorIndex, FileEntry, LocalOverlayIndex, ManifestStore,
    SymbolIndex, UnsavedBufferIndex,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const DEFAULT_TOKEN_BUDGET: usize = 8_000;
const MAX_RANGE_LINES: usize = 12;
const MAX_UNSAVED_BUFFER_BYTES: usize = 1_000_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalRequest {
    pub query: String,
    #[serde(default)]
    pub active_file: Option<String>,
    #[serde(default)]
    pub token_budget: Option<usize>,
    #[serde(default)]
    pub unsaved_buffers: Vec<UnsavedBuffer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnsavedBuffer {
    pub path: String,
    pub contents: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextPack {
    pub files: Vec<ContextFile>,
    pub omitted: Vec<OmittedContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextFile {
    pub path: String,
    pub content_hash: String,
    pub ranges: Vec<ContextRange>,
    pub why_included: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextRange {
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OmittedContext {
    pub reason: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResponse {
    pub context_pack: ContextPack,
    pub snapshot_id: String,
    pub freshness: Freshness,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Freshness {
    pub local_overlay_included: bool,
    pub unsaved_buffers_included: bool,
    pub cloud_index_lag_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalEvalCase {
    pub query: String,
    pub expected_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalEvalReport {
    pub cases_total: usize,
    pub recall_at_5_hits: usize,
    pub recall_at_5_rate_bps: u32,
    pub stale_context_count: usize,
    pub unauthorized_candidate_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FreshnessLatencyReport {
    pub path: String,
    pub query: String,
    pub save_to_search_ms: u128,
    pub found: bool,
}

#[derive(Debug, Clone)]
pub struct CodebaseRetrievalEngine {
    sync: CodebaseSyncEngine,
}

impl CodebaseRetrievalEngine {
    pub fn new(sync: CodebaseSyncEngine) -> Self {
        Self { sync }
    }

    pub fn default_engine() -> Result<Self> {
        Ok(Self::new(CodebaseSyncEngine::new(
            ManifestStore::default_store()?,
        )))
    }

    pub fn benchmark_save_to_search(
        &self,
        root: &Path,
        path: &str,
        contents: &str,
        query: &str,
    ) -> Result<FreshnessLatencyReport> {
        let absolute = root.join(path);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent)?;
        }
        let start = Instant::now();
        fs::write(&absolute, contents)?;
        self.sync.on_file_change(root, &[absolute])?;
        let response = self.search(
            root,
            RetrievalRequest {
                query: query.to_string(),
                active_file: Some(path.to_string()),
                token_budget: None,
                unsaved_buffers: Vec::new(),
            },
        )?;
        Ok(FreshnessLatencyReport {
            path: path.to_string(),
            query: query.to_string(),
            save_to_search_ms: start.elapsed().as_millis(),
            found: response.context_pack.files.iter().any(|file| file.path == path),
        })
    }

    pub fn eval_fixture(&self, root: &Path, fixture_path: &Path) -> Result<RetrievalEvalReport> {
        let cases: Vec<RetrievalEvalCase> = serde_json::from_str(&fs::read_to_string(fixture_path)?)?;
        self.eval(root, &cases)
    }

    pub fn eval(&self, root: &Path, cases: &[RetrievalEvalCase]) -> Result<RetrievalEvalReport> {
        let mut report = RetrievalEvalReport {
            cases_total: cases.len(),
            recall_at_5_hits: 0,
            recall_at_5_rate_bps: 0,
            stale_context_count: 0,
            unauthorized_candidate_count: 0,
        };
        for case in cases {
            let response = self.search(
                root,
                RetrievalRequest {
                    query: case.query.clone(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                },
            )?;
            let token = self.sync.snapshot_token(root)?.unwrap_or_else(|| {
                jcode_codebase_sync::SnapshotTokenPayload {
                    workspace_id: String::new(),
                    branch: None,
                    head_sha: None,
                    allowed_content_hashes: Vec::new(),
                    path_to_hash: Default::default(),
                    issued_at: chrono::Utc::now(),
                }
            });
            let top_paths: Vec<_> = response
                .context_pack
                .files
                .iter()
                .take(5)
                .map(|file| file.path.as_str())
                .collect();
            if case.expected_files.iter().any(|expected| top_paths.contains(&expected.as_str())) {
                report.recall_at_5_hits += 1;
            }
            for file in &response.context_pack.files {
                if !token.authorize_path_hash(&file.path, &file.content_hash)
                    && file.why_included != "unsaved buffer matches current editor state"
                {
                    report.unauthorized_candidate_count += 1;
                }
                if token
                    .path_to_hash
                    .get(&file.path)
                    .map(|hash| hash != &file.content_hash)
                    .unwrap_or(false)
                {
                    report.stale_context_count += 1;
                }
            }
        }
        if report.cases_total > 0 {
            report.recall_at_5_rate_bps = ((report.recall_at_5_hits * 10_000) / report.cases_total) as u32;
        }
        Ok(report)
    }

    pub fn search(&self, root: &Path, req: RetrievalRequest) -> Result<SearchResponse> {
        let (manifest, delta) = self.sync.open_workspace(root)?;
        let local_overlay_included = !delta.added.is_empty()
            || !delta.modified.is_empty()
            || !delta.removed.is_empty();
        let token = jcode_codebase_sync::SnapshotTokenPayload::from_manifest(&manifest);
        let mut candidates = search_unsaved_buffers(&req);
        candidates.extend(search_overlay(root, &manifest, &req)?);
        candidates.extend(search_symbols(root, &manifest, &req)?);
        candidates.extend(search_vector(root, &manifest, &req)?);
        candidates.extend(search_manifest_files(root, &manifest.files.values().collect::<Vec<_>>(), &req)?);
        let graph_neighbors = search_graph_neighbors(root, &manifest, &candidates)?;
        candidates.extend(graph_neighbors);
        candidates.retain(|candidate| {
            candidate.why == "unsaved buffer matches current editor state"
                || token.authorize_path_hash(&candidate.path, &candidate.content_hash)
        });
        rerank_candidates(&mut candidates, &req);
        let snapshot_id = format!(
            "{}:{}:{}",
            manifest.workspace_id,
            manifest.head_sha.clone().unwrap_or_default(),
            manifest.ignore_rules_hash
        );
        Ok(SearchResponse {
            context_pack: compress_candidates(candidates, req.token_budget.unwrap_or(DEFAULT_TOKEN_BUDGET)),
            snapshot_id,
            freshness: Freshness {
                local_overlay_included,
                unsaved_buffers_included: !req.unsaved_buffers.is_empty(),
                cloud_index_lag_ms: None,
            },
        })
    }
}

#[derive(Debug)]
struct Candidate {
    path: String,
    content_hash: String,
    score: usize,
    range: ContextRange,
    why: String,
}

fn search_unsaved_buffers(req: &RetrievalRequest) -> Vec<Candidate> {
    let mut index = UnsavedBufferIndex::default();
    for buffer in &req.unsaved_buffers {
        if is_safe_relative_path(&buffer.path) && is_safe_unsaved_contents(&buffer.contents) {
            index.upsert(buffer.path.clone(), &buffer.contents);
        }
    }
    index
        .search(&req.query, 20)
        .into_iter()
        .map(|hit| Candidate {
            path: hit.chunk.path,
            content_hash: hit.chunk.content_hash,
            score: hit.score + 2_000,
            range: ContextRange {
                start_line: hit.chunk.start_line,
                end_line: hit.chunk.end_line,
                text: hit.chunk.text,
            },
            why: "unsaved buffer matches current editor state".to_string(),
        })
        .collect()
}

fn search_overlay(
    root: &Path,
    manifest: &jcode_codebase_sync::Manifest,
    req: &RetrievalRequest,
) -> Result<Vec<Candidate>> {
    let overlay = LocalOverlayIndex::rebuild(root, manifest)?;
    Ok(overlay
        .search(&req.query, 20)
        .into_iter()
        .map(|hit| Candidate {
            path: hit.chunk.path,
            content_hash: hit.chunk.content_hash,
            score: hit.score + 1_000,
            range: ContextRange {
                start_line: hit.chunk.start_line,
                end_line: hit.chunk.end_line,
                text: hit.chunk.text,
            },
            why: "saved local overlay matches current snapshot".to_string(),
        })
        .collect())
}

fn search_symbols(
    root: &Path,
    manifest: &jcode_codebase_sync::Manifest,
    req: &RetrievalRequest,
) -> Result<Vec<Candidate>> {
    let index = SymbolIndex::rebuild(root, manifest)?;
    let mut candidates = Vec::new();
    for symbol in index.search(&req.query, 20) {
        let Some(entry) = manifest.files.get(&symbol.path) else {
            continue;
        };
        let text = fs::read_to_string(root.join(&symbol.path))?;
        let lines: Vec<_> = text.lines().collect();
        let start = symbol.start_line.saturating_sub(1);
        let end = (start + MAX_RANGE_LINES).min(lines.len());
        candidates.push(Candidate {
            path: symbol.path,
            content_hash: entry.content_hash.clone(),
            score: 750,
            range: ContextRange {
                start_line: start + 1,
                end_line: end,
                text: lines[start..end].join("\n"),
            },
            why: format!("symbol definition match: {}", symbol.name),
        });
    }
    Ok(candidates)
}

fn search_vector(
    root: &Path,
    manifest: &jcode_codebase_sync::Manifest,
    req: &RetrievalRequest,
) -> Result<Vec<Candidate>> {
    let overlay = LocalOverlayIndex::rebuild(root, manifest)?;
    let index = ExactVectorIndex::rebuild(&overlay);
    Ok(index
        .search(&req.query, 20)
        .into_iter()
        .map(|hit| Candidate {
            path: hit.chunk.path,
            content_hash: hit.chunk.content_hash,
            score: (hit.score * 500.0) as usize,
            range: ContextRange {
                start_line: hit.chunk.start_line,
                end_line: hit.chunk.end_line,
                text: hit.chunk.text,
            },
            why: "exact local vector match current snapshot".to_string(),
        })
        .collect())
}

fn search_graph_neighbors(
    root: &Path,
    manifest: &jcode_codebase_sync::Manifest,
    candidates: &[Candidate],
) -> Result<Vec<Candidate>> {
    let graph = DependencyGraph::rebuild(root, manifest)?;
    let candidate_paths: HashSet<_> = candidates.iter().map(|candidate| candidate.path.as_str()).collect();
    let mut neighbors = Vec::new();
    for edge in graph.edges {
        let neighbor_path = if candidate_paths.contains(edge.from.as_str()) {
            edge.to
        } else if candidate_paths.contains(edge.to.as_str()) {
            edge.from
        } else {
            continue;
        };
        let Some(entry) = manifest.files.get(&neighbor_path) else {
            continue;
        };
        let text = fs::read_to_string(root.join(&neighbor_path))?;
        let lines: Vec<_> = text.lines().take(MAX_RANGE_LINES).collect();
        neighbors.push(Candidate {
            path: neighbor_path,
            content_hash: entry.content_hash.clone(),
            score: 250,
            range: ContextRange {
                start_line: 1,
                end_line: lines.len(),
                text: lines.join("\n"),
            },
            why: "dependency graph neighbor".to_string(),
        });
    }
    Ok(neighbors)
}

fn search_manifest_files(root: &Path, files: &[&FileEntry], req: &RetrievalRequest) -> Result<Vec<Candidate>> {
    let terms = query_terms(&req.query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    for entry in files {
        let path_score = score_text(&entry.path, &terms) * 3;
        let path = root.join(&entry.path);
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let lines: Vec<_> = text.lines().collect();
        let mut best_line = None;
        let mut best_line_score = 0;
        for (index, line) in lines.iter().enumerate() {
            let score = score_text(line, &terms);
            if score > best_line_score {
                best_line_score = score;
                best_line = Some(index);
            }
        }
        let total_score = path_score + best_line_score;
        if total_score == 0 {
            continue;
        }
        let line_index = best_line.unwrap_or(0);
        let start = line_index.saturating_sub(2);
        let end = (start + MAX_RANGE_LINES).min(lines.len());
        let range_text = lines[start..end].join("\n");
        candidates.push(Candidate {
            path: entry.path.clone(),
            content_hash: entry.content_hash.clone(),
            score: total_score,
            range: ContextRange {
                start_line: start + 1,
                end_line: end,
                text: range_text,
            },
            why: if path_score > 0 && best_line_score > 0 {
                "path and content match current local snapshot".to_string()
            } else if path_score > 0 {
                "path matches current local snapshot".to_string()
            } else {
                "content matches current local snapshot".to_string()
            },
        });
    }
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    Ok(candidates)
}

fn rerank_candidates(candidates: &mut [Candidate], req: &RetrievalRequest) {
    let query = term_set(&req.query);
    for candidate in candidates.iter_mut() {
        let content = term_set(&format!("{}\n{}", candidate.path, candidate.range.text));
        let overlap = query.intersection(&content).count();
        let union = query.union(&content).count().max(1);
        candidate.score += overlap * 100 / union;
        if req.active_file.as_deref() == Some(candidate.path.as_str()) {
            candidate.score += 25;
        }
        if candidate.path.contains("/test") || candidate.path.ends_with("_test.rs") {
            candidate.score += 5;
        }
        if is_generated_or_vendor_path(&candidate.path) {
            candidate.score = candidate.score.saturating_sub(50);
        }
    }
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
}

fn term_set(text: &str) -> HashSet<String> {
    query_terms(text).into_iter().collect()
}

fn is_generated_or_vendor_path(path: &str) -> bool {
    path.contains("/generated/")
        || path.contains("/vendor/")
        || path.contains("/dist/")
        || path.ends_with(".min.js")
}

fn compress_candidates(candidates: Vec<Candidate>, token_budget: usize) -> ContextPack {
    let byte_budget = token_budget.saturating_mul(4);
    let mut used = 0;
    let mut file_index = BTreeMap::<String, usize>::new();
    let mut files: Vec<ContextFile> = Vec::new();
    let mut omitted = 0;

    for candidate in candidates {
        let cost = candidate.range.text.len();
        if used + cost > byte_budget && !files.is_empty() {
            omitted += 1;
            continue;
        }
        used += cost;
        if let Some(index) = file_index.get(&candidate.path).copied() {
            files[index].ranges.push(candidate.range);
            files[index].ranges.sort_by_key(|range| range.start_line);
            continue;
        }
        file_index.insert(candidate.path.clone(), files.len());
        files.push(ContextFile {
            path: candidate.path,
            content_hash: candidate.content_hash,
            ranges: vec![candidate.range],
            why_included: candidate.why,
        });
    }

    ContextPack {
        files,
        omitted: if omitted == 0 {
            Vec::new()
        } else {
            vec![OmittedContext {
                reason: "token budget or duplicate path".to_string(),
                count: omitted,
            }]
        },
    }
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .filter_map(|term| {
            let term = term.trim().to_lowercase();
            (term.len() >= 2).then_some(term)
        })
        .collect()
}

fn score_text(text: &str, terms: &[String]) -> usize {
    let haystack = text.to_lowercase();
    terms.iter().filter(|term| haystack.contains(term.as_str())).count()
}

fn is_safe_relative_path(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute()
        && !path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
}

fn is_safe_unsaved_contents(contents: &str) -> bool {
    contents.len() <= MAX_UNSAVED_BUFFER_BYTES && !contents.as_bytes().contains(&0)
}

pub fn root_from_context_path(path: Option<PathBuf>) -> Result<PathBuf> {
    match path {
        Some(path) => Ok(path),
        None => std::env::current_dir().context("resolve current dir"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_codebase_sync::ManifestStore;
    use tempfile::TempDir;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    fn run_git_cmd(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn retrieval_includes_dependency_graph_neighbor() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn login() {}\n");
        write(&dir.path().join("src/auth_test.rs"), "fn login_test() { login(); }\n");
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "login_test".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                },
            )
            .unwrap();
        assert!(response.context_pack.files.iter().any(|file| file.path == "src/auth.rs"));
    }

    #[test]
    fn branch_switch_retrieval_uses_current_branch_manifest() {
        let dir = TempDir::new().unwrap();
        run_git_cmd(dir.path(), &["init"]);
        run_git_cmd(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git_cmd(dir.path(), &["config", "user.name", "Test"]);
        write(&dir.path().join("src/lib.rs"), "pub fn branch_symbol() { main_only(); }\n");
        run_git_cmd(dir.path(), &["add", "."]);
        run_git_cmd(dir.path(), &["commit", "-m", "main"]);
        let store = TempDir::new().unwrap();
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let main = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "main only".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                },
            )
            .unwrap();
        assert!(main.context_pack.files[0].ranges[0].text.contains("main_only"));
        run_git_cmd(dir.path(), &["checkout", "-b", "feature"]);
        write(&dir.path().join("src/lib.rs"), "pub fn branch_symbol() { feature_only(); }\n");
        run_git_cmd(dir.path(), &["add", "."]);
        run_git_cmd(dir.path(), &["commit", "-m", "feature"]);
        let feature = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "feature only".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                },
            )
            .unwrap();
        assert!(feature.context_pack.files[0].ranges[0].text.contains("feature_only"));
        assert!(!feature.context_pack.files[0].ranges[0].text.contains("main_only"));
    }

    #[test]
    fn search_returns_matching_file_range() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn login() {\n    validate_password();\n}\n",
        );
        write(&dir.path().join("src/other.rs"), "pub fn other() {}\n");
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "login validation".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                },
            )
            .unwrap();
        assert_eq!(response.context_pack.files[0].path, "src/auth.rs");
        assert!(response.context_pack.files[0].ranges[0].text.contains("login"));
    }

    #[test]
    fn unsaved_buffer_result_is_preferred() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn login() { saved_version(); }\n");
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "unsaved_version".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: vec![UnsavedBuffer {
                        path: "src/auth.rs".to_string(),
                        contents: "pub fn login() { unsaved_version(); }\n".to_string(),
                    }],
                },
            )
            .unwrap();
        assert_eq!(response.context_pack.files[0].path, "src/auth.rs");
        assert_eq!(
            response.context_pack.files[0].why_included,
            "unsaved buffer matches current editor state"
        );
        assert!(response.context_pack.files[0].ranges[0].text.contains("unsaved_version"));
        assert!(response.freshness.unsaved_buffers_included);
    }

    #[test]
    fn benchmark_reports_save_to_search_latency() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/lib.rs"), "pub fn before() {}\n");
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        engine.sync.open_workspace(dir.path()).unwrap();
        let report = engine
            .benchmark_save_to_search(
                dir.path(),
                "src/lib.rs",
                "pub fn after_latency_probe() {}\n",
                "after latency probe",
            )
            .unwrap();
        assert_eq!(report.path, "src/lib.rs");
        assert!(report.found);
    }

    #[test]
    fn compressor_keeps_multiple_ranges_for_same_file() {
        let pack = compress_candidates(
            vec![
                Candidate {
                    path: "src/lib.rs".to_string(),
                    content_hash: "sha256:a".to_string(),
                    score: 2,
                    range: ContextRange {
                        start_line: 10,
                        end_line: 11,
                        text: "second".to_string(),
                    },
                    why: "test".to_string(),
                },
                Candidate {
                    path: "src/lib.rs".to_string(),
                    content_hash: "sha256:a".to_string(),
                    score: 1,
                    range: ContextRange {
                        start_line: 1,
                        end_line: 2,
                        text: "first".to_string(),
                    },
                    why: "test".to_string(),
                },
            ],
            1_000,
        );
        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].ranges.len(), 2);
        assert_eq!(pack.files[0].ranges[0].start_line, 1);
    }

    #[test]
    fn generated_candidate_is_penalized() {
        let mut candidates = vec![
            Candidate {
                path: "src/generated/auth.rs".to_string(),
                content_hash: "sha256:a".to_string(),
                score: 50,
                range: ContextRange {
                    start_line: 1,
                    end_line: 1,
                    text: "validate password".to_string(),
                },
                why: "test".to_string(),
            },
            Candidate {
                path: "src/auth.rs".to_string(),
                content_hash: "sha256:b".to_string(),
                score: 50,
                range: ContextRange {
                    start_line: 1,
                    end_line: 1,
                    text: "validate password".to_string(),
                },
                why: "test".to_string(),
            },
        ];
        rerank_candidates(
            &mut candidates,
            &RetrievalRequest {
                query: "validate password".to_string(),
                active_file: None,
                token_budget: None,
                unsaved_buffers: Vec::new(),
            },
        );
        assert_eq!(candidates[0].path, "src/auth.rs");
    }

    #[test]
    fn active_file_boost_reranks_candidates() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn shared_term() { auth_login(); }\n");
        write(&dir.path().join("src/other.rs"), "pub fn shared_term() { other_login(); }\n");
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "shared_term login".to_string(),
                    active_file: Some("src/other.rs".to_string()),
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                },
            )
            .unwrap();
        assert_eq!(response.context_pack.files[0].path, "src/other.rs");
    }

    #[test]
    fn eval_fixture_reports_recall_at_5() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        let fixture = dir.path().join("eval.json");
        write(&dir.path().join("src/auth.rs"), "pub fn login() { validate_password(); }\n");
        write(
            &fixture,
            r#"[{"query":"password validation","expected_files":["src/auth.rs"]}]"#,
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let report = engine.eval_fixture(dir.path(), &fixture).unwrap();
        assert_eq!(report.cases_total, 1);
        assert_eq!(report.recall_at_5_hits, 1);
        assert_eq!(report.recall_at_5_rate_bps, 10_000);
    }

    #[test]
    fn eval_reports_recall_at_5() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn login() { validate_password(); }\n");
        write(&dir.path().join("src/billing.rs"), "pub fn charge_card() {}\n");
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let report = engine
            .eval(
                dir.path(),
                &[RetrievalEvalCase {
                    query: "password validation".to_string(),
                    expected_files: vec!["src/auth.rs".to_string()],
                }],
            )
            .unwrap();
        assert_eq!(report.cases_total, 1);
        assert_eq!(report.recall_at_5_hits, 1);
        assert_eq!(report.recall_at_5_rate_bps, 10_000);
        assert_eq!(report.stale_context_count, 0);
        assert_eq!(report.unauthorized_candidate_count, 0);
    }

    #[test]
    fn unsafe_unsaved_buffer_contents_are_ignored() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn login() {}\n");
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "nul_unsaved".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: vec![UnsavedBuffer {
                        path: "src/auth.rs".to_string(),
                        contents: "fn nul_unsaved() {\0}\n".to_string(),
                    }],
                },
            )
            .unwrap();
        assert!(response.context_pack.files.is_empty());
    }

    #[test]
    fn unsafe_unsaved_buffer_path_is_ignored() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn login() {}\n");
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "secret_unsaved".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: vec![UnsavedBuffer {
                        path: "../secret.rs".to_string(),
                        contents: "fn secret_unsaved() {}\n".to_string(),
                    }],
                },
            )
            .unwrap();
        assert!(response.context_pack.files.is_empty());
    }

    #[test]
    fn overlay_result_is_preferred() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn login() {\n    validate_password();\n}\n",
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "validate_password".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                },
            )
            .unwrap();
        assert_eq!(
            response.context_pack.files[0].why_included,
            "saved local overlay matches current snapshot"
        );
    }

    #[test]
    fn deleted_file_does_not_leak_through_vector_candidates() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        let file = dir.path().join("src/secret.rs");
        write(&file, "pub fn deleted_vector_symbol() {}\n");
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let first = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "deleted vector symbol".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                },
            )
            .unwrap();
        assert_eq!(first.context_pack.files[0].path, "src/secret.rs");
        fs::remove_file(file).unwrap();
        let second = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "deleted vector symbol".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                },
            )
            .unwrap();
        assert!(second.context_pack.files.is_empty());
    }

    #[test]
    fn deleted_file_disappears_after_rescan() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        let file = dir.path().join("src/auth.rs");
        write(&file, "pub fn login() {}\n");
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let first = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "login".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                },
            )
            .unwrap();
        assert_eq!(first.context_pack.files.len(), 1);
        fs::remove_file(file).unwrap();
        let second = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "login".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                },
            )
            .unwrap();
        assert!(second.context_pack.files.is_empty());
    }
}
