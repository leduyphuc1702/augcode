use anyhow::{Context, Result};
use jcode_codebase_sync::{
    CodebaseSyncEngine, DependencyGraph, IgnoreRules, IndexSnapshot, Manifest, ManifestStore,
    UnsavedBufferIndex, build_manifest, discover_filter_hash,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub mod lexical_index;
pub mod query_planner;
use query_planner::{QueryIntent, QueryPlanner, SourceWeights};

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
    #[serde(default)]
    pub intent: Option<QueryIntent>,
    #[serde(default)]
    pub active_file: Option<String>,
    #[serde(default)]
    pub must_not_return: Vec<String>,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalEvalCategoryReport {
    pub category: String,
    pub cases_total: usize,
    pub recall_at_5_hits: usize,
    pub recall_at_5_rate_bps: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalEvalMissingCase {
    pub query: String,
    pub expected_files: Vec<String>,
    pub returned_files: Vec<String>,
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalEvalReport {
    pub cases_total: usize,
    pub recall_at_5_hits: usize,
    pub recall_at_5_rate_bps: u32,
    pub recall_at_20_hits: usize,
    pub recall_at_20_rate_bps: u32,
    pub mrr_bps: u32,
    pub stale_context_count: usize,
    pub unauthorized_candidate_count: usize,
    pub forbidden_context_count: usize,
    pub categories: Vec<RetrievalEvalCategoryReport>,
    pub missing_expected: Vec<RetrievalEvalMissingCase>,
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
            found: response
                .context_pack
                .files
                .iter()
                .any(|file| file.path == path),
        })
    }

    pub fn eval_fixture(&self, root: &Path, fixture_path: &Path) -> Result<RetrievalEvalReport> {
        let cases: Vec<RetrievalEvalCase> =
            serde_json::from_str(&fs::read_to_string(fixture_path)?)?;
        let excluded = normalize_eval_fixture_path(root, fixture_path);
        self.eval_internal(root, &cases, excluded.as_deref())
    }

    pub fn eval(&self, root: &Path, cases: &[RetrievalEvalCase]) -> Result<RetrievalEvalReport> {
        self.eval_internal(root, cases, None)
    }

    fn eval_internal(
        &self,
        root: &Path,
        cases: &[RetrievalEvalCase],
        excluded_path: Option<&str>,
    ) -> Result<RetrievalEvalReport> {
        let mut report = RetrievalEvalReport {
            cases_total: cases.len(),
            recall_at_5_hits: 0,
            recall_at_5_rate_bps: 0,
            recall_at_20_hits: 0,
            recall_at_20_rate_bps: 0,
            mrr_bps: 0,
            stale_context_count: 0,
            unauthorized_candidate_count: 0,
            forbidden_context_count: 0,
            categories: Vec::new(),
            missing_expected: Vec::new(),
        };
        let mut categories = BTreeMap::<String, (usize, usize)>::new();
        for case in cases {
            let response = self.search(
                root,
                RetrievalRequest {
                    query: case.query.clone(),
                    active_file: case.active_file.clone(),
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
            let all_paths: Vec<_> = response
                .context_pack
                .files
                .iter()
                .map(|file| file.path.as_str())
                .filter(|path| Some(*path) != excluded_path)
                .collect();
            let top_5: Vec<_> = all_paths.iter().take(5).copied().collect();
            let hit_5 = case
                .expected_files
                .iter()
                .any(|expected| top_5.contains(&expected.as_str()));
            if hit_5 {
                report.recall_at_5_hits += 1;
            }
            let top_20: Vec<_> = all_paths.iter().take(20).copied().collect();
            if case
                .expected_files
                .iter()
                .any(|expected| top_20.contains(&expected.as_str()))
            {
                report.recall_at_20_hits += 1;
            }
            let mut rank = None;
            for (i, path) in all_paths.iter().enumerate() {
                if case.expected_files.contains(&path.to_string()) {
                    rank = Some(i + 1);
                    break;
                }
            }
            if let Some(r) = rank {
                report.mrr_bps += (10_000 / r) as u32;
            } else {
                report.missing_expected.push(RetrievalEvalMissingCase {
                    query: case.query.clone(),
                    expected_files: case.expected_files.clone(),
                    returned_files: all_paths.iter().map(|path| (*path).to_string()).collect(),
                    category: case.category.clone(),
                });
            }
            if let Some(category) = &case.category {
                let entry = categories.entry(category.clone()).or_default();
                entry.0 += 1;
                if hit_5 {
                    entry.1 += 1;
                }
            }
            for file in &response.context_pack.files {
                if Some(file.path.as_str()) == excluded_path {
                    continue;
                }
                if case
                    .must_not_return
                    .iter()
                    .any(|forbidden| forbidden == &file.path)
                {
                    report.forbidden_context_count += 1;
                }
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
            report.recall_at_5_rate_bps =
                ((report.recall_at_5_hits * 10_000) / report.cases_total) as u32;
            report.recall_at_20_rate_bps =
                ((report.recall_at_20_hits * 10_000) / report.cases_total) as u32;
            report.mrr_bps = report.mrr_bps / report.cases_total as u32;
        }
        report.categories = categories
            .into_iter()
            .map(
                |(category, (cases_total, recall_at_5_hits))| RetrievalEvalCategoryReport {
                    category,
                    cases_total,
                    recall_at_5_hits,
                    recall_at_5_rate_bps: rate_bps(recall_at_5_hits, cases_total),
                },
            )
            .collect();
        Ok(report)
    }

    pub fn search(&self, root: &Path, req: RetrievalRequest) -> Result<SearchResponse> {
        let (snapshot, mut local_overlay_included) = match self.sync.index_snapshot(root)? {
            Some(snapshot) => (snapshot, false),
            None => {
                let (_manifest, delta) = self.sync.open_workspace(root)?;
                let snapshot = self
                    .sync
                    .index_snapshot(root)?
                    .ok_or_else(|| anyhow::anyhow!("codebase index snapshot was not written"))?;
                (
                    snapshot,
                    !delta.added.is_empty()
                        || !delta.modified.is_empty()
                        || !delta.removed.is_empty(),
                )
            }
        };
        let mut manifest = snapshot.manifest.clone();
        let mut token = jcode_codebase_sync::SnapshotTokenPayload::from_manifest(&manifest);
        let graph = snapshot.graph.clone();
        let graph_distances = bfs_distances(&graph, req.active_file.as_deref());
        let mut candidates = search_unsaved_buffers(&req);
        candidates.extend(search_overlay(&snapshot, &req));
        candidates.extend(search_ast_chunks(&snapshot, &req));
        candidates.extend(search_symbols(&snapshot, &req));
        candidates.extend(search_manifest_files(&snapshot, &req));
        let graph_neighbors = search_graph_neighbors_with_graph(&graph, &snapshot, &candidates);
        candidates.extend(graph_neighbors);
        let before_validation = candidates.len();
        candidates.retain(|candidate| {
            candidate.why == "unsaved buffer matches current editor state"
                || candidate_matches_disk(root, candidate).unwrap_or(false)
        });
        if before_validation > candidates.len()
            || (candidates.is_empty() && req.unsaved_buffers.is_empty())
        {
            if let Some((fallback_manifest, fallback_candidates)) =
                search_cold_or_stale_fallback(root, &req)?
            {
                manifest = fallback_manifest;
                token = jcode_codebase_sync::SnapshotTokenPayload::from_manifest(&manifest);
                candidates.extend(fallback_candidates);
                local_overlay_included = true;
            }
        }
        candidates.retain(|candidate| {
            candidate.why == "unsaved buffer matches current editor state"
                || token.authorize_path_hash(&candidate.path, &candidate.content_hash)
        });
        let plan = QueryPlanner::plan(&req.query);
        HybridReranker::new(&req, &plan.weights, &graph_distances, &plan.intent)
            .rank(&mut candidates);
        dedupe_candidates(&mut candidates);
        let snapshot_id = format!(
            "{}:{}:{}",
            manifest.workspace_id,
            manifest.head_sha.clone().unwrap_or_default(),
            manifest.ignore_rules_hash
        );
        Ok(SearchResponse {
            context_pack: compress_candidates(
                candidates,
                req.token_budget.unwrap_or(DEFAULT_TOKEN_BUDGET),
            ),
            snapshot_id,
            freshness: Freshness {
                local_overlay_included,
                unsaved_buffers_included: !req.unsaved_buffers.is_empty(),
                cloud_index_lag_ms: None,
            },
        })
    }
}

fn rate_bps(hits: usize, total: usize) -> u32 {
    if total == 0 {
        0
    } else {
        ((hits * 10_000) / total) as u32
    }
}

fn normalize_eval_fixture_path(root: &Path, fixture_path: &Path) -> Option<String> {
    let path = if fixture_path.is_absolute() {
        fixture_path.strip_prefix(root).ok()?.to_path_buf()
    } else {
        fixture_path.to_path_buf()
    };
    Some(path.to_string_lossy().replace('\\', "/"))
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

fn search_overlay(snapshot: &IndexSnapshot, req: &RetrievalRequest) -> Vec<Candidate> {
    snapshot
        .overlay
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
        .collect()
}

fn search_ast_chunks(snapshot: &IndexSnapshot, req: &RetrievalRequest) -> Vec<Candidate> {
    let terms = query_terms(&req.query);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for chunk in &snapshot.ast_chunks.chunks {
        let Some(entry) = snapshot.manifest.files.get(&chunk.path) else {
            continue;
        };
        let score = ast_chunk_score(chunk, &terms);
        if score == 0 {
            continue;
        }
        let Some(range) = ast_chunk_range(snapshot, chunk) else {
            continue;
        };
        candidates.push(Candidate {
            path: chunk.path.clone(),
            content_hash: entry.content_hash.clone(),
            score: 900 + score,
            range,
            why: format!("ast:{}:{}", chunk.kind, chunk.name),
        });
    }
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.range.start_line.cmp(&b.range.start_line))
    });
    candidates.truncate(50);
    candidates
}

fn search_symbols(snapshot: &IndexSnapshot, req: &RetrievalRequest) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for symbol in snapshot.symbols.search(&req.query, 20) {
        let Some(entry) = snapshot.manifest.files.get(&symbol.path) else {
            continue;
        };
        let Some(range) = symbol_range_from_snapshot(
            snapshot,
            &symbol.path,
            symbol.start_line,
            symbol.end_line.max(symbol.start_line),
        ) else {
            continue;
        };
        candidates.push(Candidate {
            path: symbol.path,
            content_hash: entry.content_hash.clone(),
            score: 850 + symbol.confidence as usize,
            range,
            why: format!("symbol:name:{}", symbol.name),
        });
    }
    candidates
}

fn search_graph_neighbors_with_graph(
    graph: &DependencyGraph,
    snapshot: &IndexSnapshot,
    candidates: &[Candidate],
) -> Vec<Candidate> {
    let candidate_paths: HashSet<_> = candidates
        .iter()
        .map(|candidate| candidate.path.as_str())
        .collect();
    let mut neighbors = Vec::new();
    for edge in &graph.edges {
        let neighbor_path = if candidate_paths.contains(edge.from.as_str()) {
            &edge.to
        } else if candidate_paths.contains(edge.to.as_str()) {
            &edge.from
        } else {
            continue;
        };
        let Some(entry) = snapshot.manifest.files.get(neighbor_path) else {
            continue;
        };
        let Some(doc) = snapshot.lexical.document(neighbor_path) else {
            continue;
        };
        let text = &doc.text;
        let lines: Vec<_> = text.lines().take(MAX_RANGE_LINES).collect();
        neighbors.push(Candidate {
            path: neighbor_path.clone(),
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
    neighbors
}

fn bfs_distances(graph: &DependencyGraph, start: Option<&str>) -> HashMap<String, usize> {
    let mut distances = HashMap::new();
    let Some(start) = start else {
        return distances;
    };
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((start.to_string(), 0usize));
    distances.insert(start.to_string(), 0);
    while let Some((current, dist)) = queue.pop_front() {
        for edge in &graph.edges {
            let neighbor = if edge.from == current {
                &edge.to
            } else if edge.to == current {
                &edge.from
            } else {
                continue;
            };
            if distances.contains_key(neighbor) {
                continue;
            }
            distances.insert(neighbor.clone(), dist + 1);
            queue.push_back((neighbor.clone(), dist + 1));
        }
    }
    distances
}

fn ast_chunk_score(chunk: &jcode_codebase_sync::AstChunk, terms: &[String]) -> usize {
    let mut score = score_text(&chunk.name, terms) * 80;
    score += score_text(&chunk.signature, terms) * 40;
    score += score_text(&chunk.kind, terms) * 20;
    score += score_text(&chunk.path, terms) * 80;
    if terms
        .iter()
        .any(|term| term == &chunk.name.to_lowercase() || term == &chunk.name)
    {
        score += 150;
    }
    if chunk.kind == "file_chunk" {
        score = score.saturating_sub(75);
    }
    score
}

fn ast_chunk_range(
    snapshot: &IndexSnapshot,
    chunk: &jcode_codebase_sync::AstChunk,
) -> Option<ContextRange> {
    range_from_snapshot_lines(snapshot, &chunk.path, chunk.start_line, chunk.end_line)
}

fn symbol_range_from_snapshot(
    snapshot: &IndexSnapshot,
    path: &str,
    start_line: usize,
    end_line: usize,
) -> Option<ContextRange> {
    if let Some(chunk) = snapshot.ast_chunks.chunk_for_line(path, start_line) {
        return range_from_snapshot_lines(snapshot, path, chunk.start_line, chunk.end_line);
    }
    if let Some(chunk) = snapshot.overlay.chunk_for_line(path, start_line) {
        return Some(ContextRange {
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            text: chunk.text.clone(),
        });
    }
    range_from_snapshot_lines(snapshot, path, start_line, end_line)
}

fn range_from_snapshot_lines(
    snapshot: &IndexSnapshot,
    path: &str,
    start_line: usize,
    end_line: usize,
) -> Option<ContextRange> {
    let doc = snapshot.lexical.document(path)?;
    let lines: Vec<_> = doc.text.lines().collect();
    if lines.is_empty() {
        return Some(ContextRange {
            start_line: 1,
            end_line: 1,
            text: String::new(),
        });
    }
    let start = start_line.saturating_sub(1);
    if start >= lines.len() {
        return None;
    }
    let end = end_line.max(start_line).min(lines.len()).max(start + 1);
    Some(ContextRange {
        start_line: start + 1,
        end_line: end,
        text: lines[start..end].join("\n"),
    })
}

fn search_manifest_files(snapshot: &IndexSnapshot, req: &RetrievalRequest) -> Vec<Candidate> {
    let terms = query_terms(&req.query);
    if terms.is_empty() {
        return Vec::new();
    }
    let bm25_hits = snapshot.lexical.search(&req.query, 50);
    let mut candidates = Vec::new();
    for hit in bm25_hits {
        let Some(entry) = snapshot.manifest.files.get(&hit.path) else {
            continue;
        };
        let Some(doc) = snapshot.lexical.document(&entry.path) else {
            continue;
        };
        let text = &doc.text;
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
        let path_score = score_text(&entry.path, &terms) * 3;
        let bm25_score = (hit.score * 100.0) as usize;
        let total_score = bm25_score + path_score + best_line_score;
        if total_score == 0 {
            continue;
        }
        let line_index = best_line.unwrap_or(0);
        let range = if let Some(chunk) = snapshot
            .ast_chunks
            .chunk_for_line(&entry.path, line_index + 1)
        {
            ast_chunk_range(snapshot, chunk).unwrap_or_else(|| {
                let start = line_index.saturating_sub(2);
                let end = (start + MAX_RANGE_LINES).min(lines.len());
                ContextRange {
                    start_line: start + 1,
                    end_line: end,
                    text: lines[start..end].join("\n"),
                }
            })
        } else {
            let start = line_index.saturating_sub(2);
            let end = (start + MAX_RANGE_LINES).min(lines.len());
            ContextRange {
                start_line: start + 1,
                end_line: end,
                text: lines[start..end].join("\n"),
            }
        };
        candidates.push(Candidate {
            path: entry.path.clone(),
            content_hash: entry.content_hash.clone(),
            score: total_score,
            range,
            why: if path_score > 0 && best_line_score > 0 {
                "bm25:path+content".to_string()
            } else if path_score > 0 {
                "bm25:path".to_string()
            } else {
                "bm25:content".to_string()
            },
        });
    }
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    candidates
}

fn search_cold_or_stale_fallback(
    root: &Path,
    req: &RetrievalRequest,
) -> Result<Option<(Manifest, Vec<Candidate>)>> {
    let rules = IgnoreRules::load(root)?;
    let scan = discover_filter_hash(root, &rules)?;
    if scan.files.is_empty() {
        return Ok(None);
    }
    let manifest = build_manifest(root, &rules, scan.files)?;
    let candidates = search_manifest_files_from_entries(root, &manifest, &req.query)?;
    Ok(Some((manifest, candidates)))
}

fn search_manifest_files_from_entries(
    root: &Path,
    manifest: &Manifest,
    query: &str,
) -> Result<Vec<Candidate>> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    for entry in manifest.files.values() {
        let text = fs::read_to_string(root.join(&entry.path)).unwrap_or_default();
        let path_score = score_text(&entry.path, &terms) * 3;
        let content_score = score_text(&text, &terms);
        if path_score + content_score == 0 {
            continue;
        }
        let lines: Vec<_> = text.lines().collect();
        let mut best_line = 0usize;
        let mut best_line_score = 0usize;
        for (index, line) in lines.iter().enumerate() {
            let score = score_text(line, &terms);
            if score > best_line_score {
                best_line = index;
                best_line_score = score;
            }
        }
        let start = best_line.saturating_sub(2);
        let end = (start + MAX_RANGE_LINES).min(lines.len());
        candidates.push(Candidate {
            path: entry.path.clone(),
            content_hash: entry.content_hash.clone(),
            score: path_score + content_score + best_line_score,
            range: ContextRange {
                start_line: start + 1,
                end_line: end,
                text: lines[start..end].join("\n"),
            },
            why: "targeted fallback scan matched current disk".to_string(),
        });
    }
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    candidates.truncate(50);
    Ok(candidates)
}

fn candidate_matches_disk(root: &Path, candidate: &Candidate) -> Result<bool> {
    let bytes = match fs::read(root.join(&candidate.path)) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(false),
    };
    let hash = format!("sha256:{}", jcode_codebase_sync::sha256_hex(&bytes));
    Ok(hash == candidate.content_hash)
}

struct HybridReranker<'a> {
    req: &'a RetrievalRequest,
    weights: &'a SourceWeights,
    graph_distances: &'a HashMap<String, usize>,
    intent: &'a QueryIntent,
}

impl<'a> HybridReranker<'a> {
    fn new(
        req: &'a RetrievalRequest,
        weights: &'a SourceWeights,
        graph_distances: &'a HashMap<String, usize>,
        intent: &'a QueryIntent,
    ) -> Self {
        Self {
            req,
            weights,
            graph_distances,
            intent,
        }
    }

    fn rank(&self, candidates: &mut [Candidate]) {
        let query = term_set(&self.req.query);
        for candidate in candidates.iter_mut() {
            let mut reasons = Vec::new();
            let content = term_set(&format!("{}\n{}", candidate.path, candidate.range.text));
            let overlap = query.intersection(&content).count();
            let union = query.union(&content).count().max(1);
            let overlap_score = overlap * 100 / union;
            if overlap_score > 0 {
                candidate.score += overlap_score;
                reasons.push(format!("overlap={}", overlap_score));
            }
            let source_bonus = source_weight_bonus(&candidate.why, self.weights);
            if source_bonus > 0 {
                candidate.score += source_bonus;
                reasons.push(format!("source={}", source_bonus));
            }
            let path_bonus = path_match_bonus(&candidate.path, &query);
            if path_bonus > 0 {
                candidate.score += path_bonus;
                reasons.push(format!("path={}", path_bonus));
            }
            if self.req.active_file.as_deref() == Some(candidate.path.as_str()) {
                candidate.score += 75;
                reasons.push("active_file=75".to_string());
            }
            let graph_bonus = graph_distance_bonus(
                &candidate.path,
                self.req.active_file.as_deref(),
                self.graph_distances,
            );
            if graph_bonus > 0 {
                candidate.score += graph_bonus;
                reasons.push(format!("graph={}", graph_bonus));
            }
            let package_bonus =
                same_package_bonus(&candidate.path, self.req.active_file.as_deref());
            if package_bonus > 0 {
                candidate.score += package_bonus;
                reasons.push(format!("package={}", package_bonus));
            }
            let intent_bonus = test_config_bonus(&candidate.path, self.intent);
            if intent_bonus > 0 {
                candidate.score += intent_bonus;
                reasons.push(format!("intent={}", intent_bonus));
            }
            if is_generated_or_vendor_path(&candidate.path) {
                candidate.score = candidate.score.saturating_sub(100);
                reasons.push("generated_penalty=100".to_string());
            }
            if !reasons.is_empty() {
                candidate.why = format!("{}; {}", candidate.why, reasons.join(","));
            }
        }
        candidates.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.range.start_line.cmp(&b.range.start_line))
        });
    }
}

#[cfg(test)]
fn rerank_candidates(
    candidates: &mut [Candidate],
    req: &RetrievalRequest,
    weights: &SourceWeights,
    graph_distances: &HashMap<String, usize>,
    intent: &QueryIntent,
) {
    HybridReranker::new(req, weights, graph_distances, intent).rank(candidates);
}

fn graph_distance_bonus(
    candidate_path: &str,
    active_file: Option<&str>,
    distances: &HashMap<String, usize>,
) -> usize {
    let Some(active) = active_file else {
        return 0;
    };
    if candidate_path == active {
        return 0;
    }
    let Some(dist) = distances.get(candidate_path) else {
        return 0;
    };
    match *dist {
        1 => 40,
        2 => 20,
        3 => 10,
        _ => 0,
    }
}

fn same_package_bonus(candidate_path: &str, active_file: Option<&str>) -> usize {
    let Some(active) = active_file else {
        return 0;
    };
    let candidate_parts: Vec<_> = candidate_path.split('/').collect();
    let active_parts: Vec<_> = active.split('/').collect();
    let common = candidate_parts
        .iter()
        .zip(active_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    common.saturating_sub(1) * 10
}

fn path_match_bonus(candidate_path: &str, query_terms: &HashSet<String>) -> usize {
    let path_terms = term_set(candidate_path);
    let matches = query_terms.intersection(&path_terms).count();
    let basename = Path::new(candidate_path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(candidate_path)
        .to_lowercase();
    let exact_basename = query_terms.contains(&basename);
    let stem = basename
        .split('.')
        .next()
        .unwrap_or(basename.as_str())
        .to_string();
    let exact_stem = query_terms.contains(&stem);
    matches * 120 + usize::from(exact_basename) * 1_000 + usize::from(exact_stem) * 750
}

fn test_config_bonus(candidate_path: &str, intent: &query_planner::QueryIntent) -> usize {
    let is_test = candidate_path.contains("/test") || candidate_path.ends_with("_test.rs");
    let is_config = candidate_path.ends_with(".toml")
        || candidate_path.ends_with(".yaml")
        || candidate_path.ends_with(".yml")
        || candidate_path.ends_with(".json");
    match intent {
        query_planner::QueryIntent::Test if is_test => 30,
        query_planner::QueryIntent::Test if is_config => 15,
        query_planner::QueryIntent::Debug if is_test => 15,
        query_planner::QueryIntent::Review if is_config => 10,
        _ if is_test => 5,
        _ => 0,
    }
}

fn source_weight_bonus(why: &str, weights: &SourceWeights) -> usize {
    if why == "unsaved buffer matches current editor state" {
        weights.unsaved_buffer.max(0) as usize
    } else if why.starts_with("saved local overlay") {
        weights.overlay.max(0) as usize
    } else if why.starts_with("symbol:name:") {
        weights.symbol.max(0) as usize
    } else if why.starts_with("ast:") {
        weights.symbol.max(0) as usize
    } else if why == "exact local vector match current snapshot" {
        weights.vector.max(0) as usize
    } else if why.starts_with("dependency graph neighbor") {
        weights.graph_neighbor.max(0) as usize
    } else {
        weights.manifest.max(0) as usize
    }
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

fn dedupe_candidates(candidates: &mut Vec<Candidate>) {
    let mut deduped = BTreeMap::<(String, usize, usize), Candidate>::new();
    for candidate in candidates.drain(..) {
        let key = (
            candidate.path.clone(),
            candidate.range.start_line,
            candidate.range.end_line,
        );
        match deduped.get_mut(&key) {
            Some(existing) if candidate.score > existing.score => {
                let previous_why = existing.why.clone();
                *existing = candidate;
                existing.why = format!("{}; dedup:{}", existing.why, previous_why);
            }
            Some(existing) => {
                existing.why = format!("{}; dedup:{}", existing.why, candidate.why);
            }
            None => {
                deduped.insert(key, candidate);
            }
        }
    }
    candidates.extend(deduped.into_values());
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.range.start_line.cmp(&b.range.start_line))
    });
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
    terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count()
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
    use jcode_codebase_sync::{IndexStore, ManifestStore};
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
        write(
            &dir.path().join("src/auth_test.rs"),
            "fn login_test() { login(); }\n",
        );
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
        assert!(
            response
                .context_pack
                .files
                .iter()
                .any(|file| file.path == "src/auth.rs")
        );
    }

    #[test]
    fn branch_switch_retrieval_uses_current_branch_manifest() {
        let dir = TempDir::new().unwrap();
        run_git_cmd(dir.path(), &["init"]);
        run_git_cmd(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git_cmd(dir.path(), &["config", "user.name", "Test"]);
        write(
            &dir.path().join("src/lib.rs"),
            "pub fn branch_symbol() { main_only(); }\n",
        );
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
        assert!(
            main.context_pack.files[0].ranges[0]
                .text
                .contains("main_only")
        );
        run_git_cmd(dir.path(), &["checkout", "-b", "feature"]);
        write(
            &dir.path().join("src/lib.rs"),
            "pub fn branch_symbol() { feature_only(); }\n",
        );
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
        assert!(
            feature.context_pack.files[0].ranges[0]
                .text
                .contains("feature_only")
        );
        assert!(
            !feature.context_pack.files[0].ranges[0]
                .text
                .contains("main_only")
        );
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
        assert!(
            response.context_pack.files[0].ranges[0]
                .text
                .contains("login")
        );
    }

    #[test]
    fn hot_query_reuses_persisted_index_snapshot() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn login() {\n    validate_password();\n}\n",
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        engine
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
        let index_path = IndexStore::new(store.path().to_path_buf()).snapshot_path(dir.path());
        let before = fs::read_to_string(&index_path).unwrap();
        engine
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
        let after = fs::read_to_string(&index_path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn unsaved_buffer_result_is_preferred() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn login() { saved_version(); }\n",
        );
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
        assert!(
            response.context_pack.files[0]
                .why_included
                .starts_with("unsaved buffer matches current editor state")
        );
        assert!(
            response.context_pack.files[0].ranges[0]
                .text
                .contains("unsaved_version")
        );
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
            &SourceWeights::default(),
            &HashMap::new(),
            &query_planner::QueryIntent::General,
        );
        assert_eq!(candidates[0].path, "src/auth.rs");
    }

    #[test]
    fn active_file_boost_reranks_candidates() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn shared_term() { auth_login(); }\n",
        );
        write(
            &dir.path().join("src/other.rs"),
            "pub fn shared_term() { other_login(); }\n",
        );
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
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn login() { validate_password(); }\n",
        );
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
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn login() { validate_password(); }\n",
        );
        write(
            &dir.path().join("src/billing.rs"),
            "pub fn charge_card() {}\n",
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let report = engine
            .eval(
                dir.path(),
                &[RetrievalEvalCase {
                    query: "password validation".to_string(),
                    expected_files: vec!["src/auth.rs".to_string()],
                    intent: None,
                    active_file: None,
                    must_not_return: Vec::new(),
                    category: None,
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
    fn eval_real_repo_recall_at_5_above_threshold() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let store = TempDir::new().unwrap();
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let fixture = root.join("fixtures").join("retrieval_eval.json");
        if !fixture.exists() {
            return;
        }
        let report = engine.eval_fixture(&root, &fixture).unwrap();
        assert!(
            report.cases_total >= 50,
            "retrieval eval fixture must contain at least 50 cases; got {}",
            report.cases_total
        );
        assert!(
            report.recall_at_5_rate_bps >= 8_500,
            "Recall@5 {} bps below 85% threshold. cases={} hits={} missing={:?}",
            report.recall_at_5_rate_bps,
            report.cases_total,
            report.recall_at_5_hits,
            report.missing_expected
        );
        assert!(
            report.recall_at_20_rate_bps >= 9_500,
            "Recall@20 {} bps below 95% threshold. cases={} hits={} missing={:?}",
            report.recall_at_20_rate_bps,
            report.cases_total,
            report.recall_at_20_hits,
            report.missing_expected
        );
        assert_eq!(
            report.stale_context_count, 0,
            "stale context leaked into eval"
        );
        assert_eq!(
            report.unauthorized_candidate_count, 0,
            "unauthorized context leaked into eval"
        );
        assert_eq!(
            report.forbidden_context_count, 0,
            "forbidden context leaked into eval"
        );
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
        assert_eq!(response.context_pack.files[0].path, "src/auth.rs");
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
