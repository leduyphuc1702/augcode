use anyhow::{Context, Result};
use jcode_code_intel::{
    CodeIntelAdapter, CodeIntelConfidence, CodeIntelEdgeKind, CodeIntelFile, CodeIntelManifest,
    CodeIntelNodeKind, CodeIntelSnapshot, CompositeCodeIntelAdapter,
};
use jcode_codebase_sync::{
    CodebaseSyncEngine, DependencyEdge, DependencyGraph, ExactVectorIndex, GraphEdgeKind,
    GraphNode, GraphNodeKind, IgnoreRules, IndexSnapshot, Manifest, ManifestStore,
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
const MAX_GRAPH_NEIGHBORS: usize = 24;
const MAX_GRAPH_DISTANCE_DEPTH: usize = 3;
const SEMANTIC_SCORE_SCALE: f32 = 1_000.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalRequest {
    pub query: String,
    #[serde(default)]
    pub active_file: Option<String>,
    #[serde(default)]
    pub token_budget: Option<usize>,
    #[serde(default)]
    pub unsaved_buffers: Vec<UnsavedBuffer>,
    #[serde(default)]
    pub include_trace: bool,
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
    #[serde(default)]
    pub score: usize,
    #[serde(default)]
    pub token_estimate: usize,
    #[serde(default)]
    pub source: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<RetrievalTrace>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalTrace {
    pub query: String,
    pub token_budget: usize,
    pub token_used: usize,
    pub candidate_count: usize,
    pub returned_count: usize,
    pub omitted_count: usize,
    pub candidates: Vec<RetrievalTraceCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalTraceCandidate {
    pub path: String,
    pub source: String,
    pub raw_score: usize,
    pub final_score: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub token_estimate: usize,
    pub reason: String,
    pub omitted_reason: Option<String>,
    #[serde(default)]
    pub graph_path: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_kind: Option<String>,
    #[serde(default)]
    pub rerank_reasons: Vec<String>,
    #[serde(default)]
    pub dedupe_key: String,
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
pub struct RetrievalEvalSourceReport {
    pub source: String,
    pub returned_count: usize,
    pub token_estimate: usize,
    pub waste_token_estimate: usize,
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
    pub precision_at_5_bps: u32,
    pub precision_at_20_bps: u32,
    pub mrr_bps: u32,
    pub context_token_estimate: usize,
    pub context_waste_token_estimate: usize,
    pub stale_context_count: usize,
    pub unauthorized_candidate_count: usize,
    pub forbidden_context_count: usize,
    pub graph_contribution_rate_bps: u32,
    pub impacted_test_recall_bps: u32,
    pub categories: Vec<RetrievalEvalCategoryReport>,
    pub sources: Vec<RetrievalEvalSourceReport>,
    #[serde(default)]
    pub recall_at_5_misses: Vec<RetrievalEvalMissingCase>,
    pub missing_expected: Vec<RetrievalEvalMissingCase>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FreshnessLatencyReport {
    pub path: String,
    pub query: String,
    pub save_to_search_ms: u128,
    pub found: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImpactDirection {
    Upstream,
    Downstream,
    #[default]
    Both,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImpactRequest {
    #[serde(default)]
    pub target_path: Option<String>,
    #[serde(default)]
    pub symbol_name: Option<String>,
    #[serde(default)]
    pub direction: ImpactDirection,
    #[serde(default)]
    pub include_tests: bool,
    #[serde(default)]
    pub max_depth: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImpactResponse {
    pub target_path: Option<String>,
    pub symbol_name: Option<String>,
    pub affected: Vec<ImpactItem>,
    pub dependency_paths: Vec<ImpactPath>,
    pub related_tests: Vec<String>,
    pub risk_level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImpactItem {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    pub kind: String,
    pub depth: usize,
    pub via: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImpactPath {
    pub nodes: Vec<String>,
    pub edges: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteMapRequest {
    #[serde(default)]
    pub route: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteMapResponse {
    pub routes: Vec<RouteMapEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteMapEntry {
    pub route: String,
    pub kind: String,
    pub handlers: Vec<RouteEndpoint>,
    pub consumers: Vec<RouteEndpoint>,
    pub downstream: Vec<RouteEndpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteEndpoint {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangeAnalysisRequest {
    #[serde(default = "default_true")]
    pub include_untracked: bool,
    #[serde(default)]
    pub include_tests: bool,
}

impl Default for ChangeAnalysisRequest {
    fn default() -> Self {
        Self {
            include_untracked: true,
            include_tests: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangeAnalysisResponse {
    pub changed_files: Vec<ChangedFile>,
    pub changed_symbols: Vec<ChangedSymbol>,
    pub impacted: Vec<ChangeImpact>,
    pub suggested_tests: Vec<String>,
    pub suggested_checks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangedSymbol {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangeImpact {
    pub path: String,
    pub risk_level: String,
    pub affected_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalUsageTrace {
    pub timestamp: String,
    pub session_id: String,
    pub message_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub event_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_label: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub context_token_estimate: usize,
    #[serde(default)]
    pub context_used_token_estimate: usize,
    #[serde(default)]
    pub context_waste_after_turn_bps: u32,
    #[serde(default)]
    pub edit_hit_rate_bps: u32,
    #[serde(default)]
    pub test_hit_rate_bps: u32,
    #[serde(default)]
    pub retrieval_to_edit_distance: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalUsageSummary {
    pub traces_total: usize,
    pub retrieval_context_events: usize,
    pub tool_call_events: usize,
    pub read_events: usize,
    pub edit_events: usize,
    pub test_events: usize,
    pub check_events: usize,
    pub context_token_estimate: usize,
    pub context_used_token_estimate: usize,
    pub context_waste_after_turn_bps: u32,
    pub edit_hit_rate_bps: u32,
    pub test_hit_rate_bps: u32,
    pub retrieval_to_edit_distance: Option<usize>,
}

fn default_true() -> bool {
    true
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
                include_trace: false,
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
            precision_at_5_bps: 0,
            precision_at_20_bps: 0,
            mrr_bps: 0,
            context_token_estimate: 0,
            context_waste_token_estimate: 0,
            stale_context_count: 0,
            unauthorized_candidate_count: 0,
            forbidden_context_count: 0,
            graph_contribution_rate_bps: 0,
            impacted_test_recall_bps: 0,
            categories: Vec::new(),
            sources: Vec::new(),
            recall_at_5_misses: Vec::new(),
            missing_expected: Vec::new(),
        };
        let mut categories = BTreeMap::<String, (usize, usize)>::new();
        let mut source_reports = BTreeMap::<String, RetrievalEvalSourceReport>::new();
        let mut graph_hit_cases = 0usize;
        let mut test_expected_cases = 0usize;
        let mut test_expected_hits = 0usize;
        let (snapshot, local_overlay_included) = self.ensure_snapshot(root)?;
        let snapshot = snapshot_with_code_intel(root, &snapshot);
        let token = jcode_codebase_sync::SnapshotTokenPayload::from_manifest(&snapshot.manifest);
        for case in cases {
            let response = self.search_snapshot(
                root,
                &snapshot,
                RetrievalRequest {
                    query: case.query.clone(),
                    active_file: case.active_file.clone(),
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                    include_trace: false,
                },
                local_overlay_included,
                false,
            )?;
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
            } else {
                report.recall_at_5_misses.push(RetrievalEvalMissingCase {
                    query: case.query.clone(),
                    expected_files: case.expected_files.clone(),
                    returned_files: top_5.iter().map(|path| (*path).to_string()).collect(),
                    category: case.category.clone(),
                });
            }
            let top_20: Vec<_> = all_paths.iter().take(20).copied().collect();
            if case
                .expected_files
                .iter()
                .any(|expected| top_20.contains(&expected.as_str()))
            {
                report.recall_at_20_hits += 1;
            }
            let graph_hit = response
                .context_pack
                .files
                .iter()
                .filter(|file| {
                    file.source.contains("graph") || file.source.contains("related_test")
                })
                .any(|file| case.expected_files.contains(&file.path));
            if graph_hit {
                graph_hit_cases += 1;
            }
            let expects_test = case.expected_files.iter().any(|path| is_test_path(path));
            if expects_test {
                test_expected_cases += 1;
                if all_paths
                    .iter()
                    .any(|path| case.expected_files.contains(&path.to_string()))
                {
                    test_expected_hits += 1;
                }
            }
            report.precision_at_5_bps += precision_bps(&top_5, &case.expected_files);
            report.precision_at_20_bps += precision_bps(&top_20, &case.expected_files);
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
                report.context_token_estimate += file.token_estimate;
                if !case.expected_files.contains(&file.path) {
                    report.context_waste_token_estimate += file.token_estimate;
                }
                for source in file.source.split(',') {
                    let entry = source_reports.entry(source.to_string()).or_insert(
                        RetrievalEvalSourceReport {
                            source: source.to_string(),
                            returned_count: 0,
                            token_estimate: 0,
                            waste_token_estimate: 0,
                        },
                    );
                    entry.returned_count += 1;
                    entry.token_estimate += file.token_estimate;
                    if !case.expected_files.contains(&file.path) {
                        entry.waste_token_estimate += file.token_estimate;
                    }
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
                    && file.source != "repo_map"
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
        report.recall_at_5_rate_bps = rate_bps(report.recall_at_5_hits, report.cases_total);
        report.recall_at_20_rate_bps = rate_bps(report.recall_at_20_hits, report.cases_total);
        report.precision_at_5_bps = average_bps(report.precision_at_5_bps, report.cases_total);
        report.precision_at_20_bps = average_bps(report.precision_at_20_bps, report.cases_total);
        report.mrr_bps = average_bps(report.mrr_bps, report.cases_total);
        report.graph_contribution_rate_bps = rate_bps(graph_hit_cases, report.cases_total);
        report.impacted_test_recall_bps = rate_bps(test_expected_hits, test_expected_cases);
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
        report.sources = source_reports.into_values().collect();
        Ok(report)
    }

    pub fn search(&self, root: &Path, req: RetrievalRequest) -> Result<SearchResponse> {
        let (snapshot, local_overlay_included) = self.ensure_snapshot(root)?;
        let snapshot = snapshot_with_code_intel(root, &snapshot);
        self.search_snapshot(root, &snapshot, req, local_overlay_included, true)
    }

    pub fn impact(&self, root: &Path, req: ImpactRequest) -> Result<ImpactResponse> {
        let (snapshot, _) = self.ensure_snapshot(root)?;
        let snapshot = snapshot_with_code_intel(root, &snapshot);
        Ok(impact_snapshot(&snapshot, req))
    }

    pub fn route_map(&self, root: &Path, req: RouteMapRequest) -> Result<RouteMapResponse> {
        let (snapshot, _) = self.ensure_snapshot(root)?;
        let snapshot = snapshot_with_code_intel(root, &snapshot);
        Ok(route_map_snapshot(&snapshot, req))
    }

    pub fn analyze_changes(
        &self,
        root: &Path,
        req: ChangeAnalysisRequest,
    ) -> Result<ChangeAnalysisResponse> {
        let (snapshot, _) = self.ensure_snapshot(root)?;
        let snapshot = snapshot_with_code_intel(root, &snapshot);
        analyze_changes_snapshot(root, &snapshot, req)
    }

    fn ensure_snapshot(&self, root: &Path) -> Result<(IndexSnapshot, bool)> {
        Ok(match self.sync.index_snapshot(root)? {
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
        })
    }

    fn search_snapshot(
        &self,
        root: &Path,
        snapshot: &IndexSnapshot,
        req: RetrievalRequest,
        mut local_overlay_included: bool,
        validate_disk: bool,
    ) -> Result<SearchResponse> {
        let mut manifest = snapshot.manifest.clone();
        let mut token = jcode_codebase_sync::SnapshotTokenPayload::from_manifest(&manifest);
        let graph = &snapshot.graph;
        let graph_distances = bfs_distances(graph, req.active_file.as_deref());
        let plan = QueryPlanner::plan(&req.query);
        let mut candidates = search_unsaved_buffers(&req);
        candidates.extend(search_exact_files(snapshot, &req));
        candidates.extend(search_symbols(snapshot, &req));
        candidates.extend(search_overlay(snapshot, &req));
        candidates.extend(search_ast_chunks(snapshot, &req));
        candidates.extend(search_semantic(snapshot, &req));
        candidates.extend(search_manifest_files(snapshot, &req));
        let graph_neighbors = search_graph_neighbors_with_graph(graph, snapshot, &candidates);
        candidates.extend(graph_neighbors);
        candidates.extend(search_related_tests_with_graph(
            graph,
            snapshot,
            &candidates,
            &plan.intent,
        ));
        candidates.extend(search_repo_map(snapshot, &req));
        if validate_disk {
            let before_validation = candidates.len();
            let mut disk_hash_cache = HashMap::<String, Option<String>>::new();
            candidates.retain(|candidate| {
                candidate.is_virtual()
                    || candidate.why == "unsaved buffer matches current editor state"
                    || candidate_matches_disk_cached(root, candidate, &mut disk_hash_cache)
                        .unwrap_or(false)
            });
            if (before_validation > candidates.len()
                || (candidates.is_empty() && req.unsaved_buffers.is_empty()))
                && let Some((fallback_manifest, fallback_candidates)) =
                    search_cold_or_stale_fallback(root, &req)?
            {
                manifest = fallback_manifest;
                token = jcode_codebase_sync::SnapshotTokenPayload::from_manifest(&manifest);
                candidates.extend(fallback_candidates);
                local_overlay_included = true;
            }
        }
        candidates.retain(|candidate| {
            candidate.is_virtual()
                || candidate.why == "unsaved buffer matches current editor state"
                || token.authorize_path_hash(&candidate.path, &candidate.content_hash)
        });
        HybridReranker::new(&req, &plan.weights, &graph_distances, &plan.intent)
            .rank(&mut candidates);
        dedupe_candidates(&mut candidates);
        let token_budget = req.token_budget.unwrap_or(DEFAULT_TOKEN_BUDGET);
        let (context_pack, trace_candidates, token_used, omitted_count) =
            compress_candidates_with_trace(candidates, token_budget, req.include_trace);
        let snapshot_id = format!(
            "{}:{}:{}",
            manifest.workspace_id,
            manifest.head_sha.clone().unwrap_or_default(),
            manifest.ignore_rules_hash
        );
        Ok(SearchResponse {
            context_pack,
            snapshot_id,
            freshness: Freshness {
                local_overlay_included,
                unsaved_buffers_included: !req.unsaved_buffers.is_empty(),
                cloud_index_lag_ms: None,
            },
            trace: req.include_trace.then_some(RetrievalTrace {
                query: req.query,
                token_budget,
                token_used,
                candidate_count: trace_candidates.len(),
                returned_count: trace_candidates
                    .iter()
                    .filter(|candidate| candidate.omitted_reason.is_none())
                    .count(),
                omitted_count,
                candidates: trace_candidates,
            }),
        })
    }
}

fn rate_bps(hits: usize, total: usize) -> u32 {
    hits.checked_mul(10_000)
        .and_then(|scaled| scaled.checked_div(total))
        .map(|rate| rate as u32)
        .unwrap_or(0)
}

fn average_bps(total_bps: u32, total: usize) -> u32 {
    u32::try_from(total)
        .ok()
        .and_then(|total| total_bps.checked_div(total))
        .unwrap_or(0)
}

fn precision_bps(paths: &[&str], expected_files: &[String]) -> u32 {
    if paths.is_empty() || expected_files.is_empty() {
        return 0;
    }
    let hits = paths
        .iter()
        .filter(|path| expected_files.iter().any(|expected| expected == **path))
        .count();
    rate_bps(hits, paths.len().min(expected_files.len()))
}

fn normalize_eval_fixture_path(root: &Path, fixture_path: &Path) -> Option<String> {
    let path = if fixture_path.is_absolute() {
        fixture_path.strip_prefix(root).ok()?.to_path_buf()
    } else {
        fixture_path.to_path_buf()
    };
    Some(path.to_string_lossy().replace('\\', "/"))
}

fn snapshot_with_code_intel(root: &Path, snapshot: &IndexSnapshot) -> IndexSnapshot {
    let mut snapshot = snapshot.clone();
    if snapshot.vector.is_empty() {
        snapshot.vector = ExactVectorIndex::rebuild(&snapshot.overlay);
    }
    let manifest = code_intel_manifest(&snapshot.manifest);
    let adapter = CompositeCodeIntelAdapter::default();
    if let Ok(intel) = adapter.index(root, &manifest) {
        merge_code_intel_into_graph(&mut snapshot.graph, &intel);
    }
    snapshot
}

fn code_intel_manifest(manifest: &Manifest) -> CodeIntelManifest {
    CodeIntelManifest {
        files: manifest
            .files
            .iter()
            .map(|(path, entry)| {
                (
                    path.clone(),
                    CodeIntelFile {
                        path: entry.path.clone(),
                        language: entry.language.clone(),
                    },
                )
            })
            .collect(),
    }
}

fn merge_code_intel_into_graph(graph: &mut DependencyGraph, intel: &CodeIntelSnapshot) {
    let mut nodes: HashSet<_> = graph.nodes.iter().map(|node| node.id.clone()).collect();
    for symbol in &intel.symbols {
        if nodes.insert(symbol.id.clone()) {
            graph.nodes.push(GraphNode {
                id: symbol.id.clone(),
                label: symbol.id.clone(),
                kind: GraphNodeKind::Symbol,
            });
        }
    }
    for edge in &intel.edges {
        let from_kind = code_intel_node_kind(edge.from_kind);
        let to_kind = code_intel_node_kind(edge.to_kind);
        let edge_kind = code_intel_edge_kind(edge.kind);
        if nodes.insert(edge.from.clone()) {
            graph.nodes.push(GraphNode {
                id: edge.from.clone(),
                label: edge.from.clone(),
                kind: from_kind,
            });
        }
        if nodes.insert(edge.to.clone()) {
            graph.nodes.push(GraphNode {
                id: edge.to.clone(),
                label: edge.to.clone(),
                kind: to_kind,
            });
        }
        graph.edges.push(DependencyEdge {
            from: edge.from.clone(),
            to: edge.to.clone(),
            kind: edge_kind.as_str().to_string(),
            from_kind,
            to_kind,
            edge_kind,
            confidence: code_intel_confidence(edge.confidence).to_string(),
        });
    }
}

fn code_intel_node_kind(kind: CodeIntelNodeKind) -> GraphNodeKind {
    match kind {
        CodeIntelNodeKind::File => GraphNodeKind::File,
        CodeIntelNodeKind::Symbol => GraphNodeKind::Symbol,
        CodeIntelNodeKind::Package => GraphNodeKind::Package,
    }
}

fn code_intel_edge_kind(kind: CodeIntelEdgeKind) -> GraphEdgeKind {
    match kind {
        CodeIntelEdgeKind::Definition => GraphEdgeKind::Definition,
        CodeIntelEdgeKind::Reference => GraphEdgeKind::Reference,
        CodeIntelEdgeKind::Call => GraphEdgeKind::Calls,
        CodeIntelEdgeKind::Implements => GraphEdgeKind::Implements,
        CodeIntelEdgeKind::Overrides => GraphEdgeKind::Overrides,
        CodeIntelEdgeKind::TypeDependency => GraphEdgeKind::TypeDependency,
    }
}

fn code_intel_confidence(confidence: CodeIntelConfidence) -> &'static str {
    confidence.as_str()
}

fn impact_snapshot(snapshot: &IndexSnapshot, req: ImpactRequest) -> ImpactResponse {
    let target_path = resolve_target_path(snapshot, &req);
    let max_depth = req
        .max_depth
        .unwrap_or(1)
        .clamp(1, MAX_GRAPH_DISTANCE_DEPTH);
    let mut affected = Vec::new();
    let mut dependency_paths = Vec::new();
    if let Some(target) = &target_path {
        let mut queue = std::collections::VecDeque::new();
        let mut seen = HashSet::new();
        queue.push_back((
            target.clone(),
            0usize,
            vec![target.clone()],
            Vec::<String>::new(),
        ));
        seen.insert(target.clone());
        while let Some((node, depth, nodes, edges)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            for edge in graph_edges_for_direction(&snapshot.graph, &node, req.direction) {
                let next = if edge.from == node {
                    edge.to.clone()
                } else {
                    edge.from.clone()
                };
                if !seen.insert(next.clone()) {
                    continue;
                }
                let mut next_nodes = nodes.clone();
                next_nodes.push(next.clone());
                let mut next_edges = edges.clone();
                next_edges.push(edge.normalized_kind().as_str().to_string());
                queue.push_back((
                    next.clone(),
                    depth + 1,
                    next_nodes.clone(),
                    next_edges.clone(),
                ));
                if let Some(path) = graph_node_file_path(&next, snapshot) {
                    affected.push(ImpactItem {
                        symbol: best_symbol_for_path(snapshot, &path),
                        kind: graph_node_kind_label(&next, snapshot),
                        path,
                        depth: depth + 1,
                        via: edge.normalized_kind().as_str().to_string(),
                    });
                    dependency_paths.push(ImpactPath {
                        nodes: next_nodes,
                        edges: next_edges,
                    });
                }
                if affected.len() >= MAX_GRAPH_NEIGHBORS {
                    break;
                }
            }
        }
    }
    if affected.is_empty()
        && let Some(symbol_name) = &req.symbol_name
    {
        affected.extend(fallback_lexical_references(
            snapshot,
            symbol_name,
            target_path.as_deref(),
        ));
    }
    dedupe_impact_items(&mut affected);
    let affected_paths: HashSet<_> = affected.iter().map(|item| item.path.as_str()).collect();
    let mut related_tests = if req.include_tests {
        related_tests_for_paths(
            &snapshot.graph,
            target_path
                .iter()
                .map(String::as_str)
                .chain(affected_paths.iter().copied()),
        )
    } else {
        Vec::new()
    };
    related_tests.sort();
    related_tests.dedup();
    ImpactResponse {
        target_path,
        symbol_name: req.symbol_name,
        risk_level: risk_level(affected.len(), related_tests.len()),
        affected,
        dependency_paths,
        related_tests,
    }
}

fn resolve_target_path(snapshot: &IndexSnapshot, req: &ImpactRequest) -> Option<String> {
    if let Some(path) = &req.target_path
        && snapshot.manifest.files.contains_key(path)
    {
        return Some(path.clone());
    }
    let symbol_name = req.symbol_name.as_deref()?;
    let normalized = normalize_identifier(symbol_name);
    snapshot
        .symbols
        .symbols
        .iter()
        .filter(|symbol| {
            req.target_path
                .as_deref()
                .map(|path| path == symbol.path)
                .unwrap_or(true)
        })
        .filter(|symbol| normalize_identifier(&symbol.name) == normalized)
        .map(|symbol| symbol.path.clone())
        .next()
        .or_else(|| {
            snapshot
                .symbols
                .symbols
                .iter()
                .find(|symbol| normalize_identifier(&symbol.name).contains(&normalized))
                .map(|symbol| symbol.path.clone())
        })
}

fn graph_edges_for_direction<'a>(
    graph: &'a DependencyGraph,
    node: &str,
    direction: ImpactDirection,
) -> Vec<&'a jcode_codebase_sync::DependencyEdge> {
    graph
        .edges
        .iter()
        .filter(|edge| !edge.matches_kind(GraphEdgeKind::Contains))
        .filter(|edge| match direction {
            ImpactDirection::Upstream => edge.to == node,
            ImpactDirection::Downstream => edge.from == node,
            ImpactDirection::Both => edge.from == node || edge.to == node,
        })
        .collect()
}

fn graph_node_file_path(node: &str, snapshot: &IndexSnapshot) -> Option<String> {
    if snapshot.manifest.files.contains_key(node) {
        return Some(node.to_string());
    }
    if let Some(rest) = node.strip_prefix("symbol:") {
        let mut parts = rest.split(':');
        let path = parts.next()?;
        if snapshot.manifest.files.contains_key(path) {
            return Some(path.to_string());
        }
    }
    None
}

fn graph_node_kind_label(node: &str, snapshot: &IndexSnapshot) -> String {
    snapshot
        .graph
        .nodes
        .iter()
        .find(|candidate| candidate.id == node)
        .map(|candidate| candidate.kind.as_str().to_string())
        .unwrap_or_else(|| {
            if is_test_path(node) {
                "test".to_string()
            } else {
                "file".to_string()
            }
        })
}

fn best_symbol_for_path(snapshot: &IndexSnapshot, path: &str) -> Option<String> {
    snapshot
        .symbols
        .symbols
        .iter()
        .find(|symbol| symbol.path == path)
        .map(|symbol| symbol.name.clone())
}

fn fallback_lexical_references(
    snapshot: &IndexSnapshot,
    symbol_name: &str,
    target_path: Option<&str>,
) -> Vec<ImpactItem> {
    let needle = symbol_name.to_lowercase();
    snapshot
        .manifest
        .files
        .keys()
        .filter(|path| Some(path.as_str()) != target_path)
        .filter_map(|path| {
            let doc = snapshot.lexical.document(path)?;
            doc.text
                .to_lowercase()
                .contains(&needle)
                .then(|| ImpactItem {
                    path: path.clone(),
                    symbol: best_symbol_for_path(snapshot, path),
                    kind: if is_test_path(path) { "test" } else { "file" }.to_string(),
                    depth: 1,
                    via: "lexical_reference".to_string(),
                })
        })
        .take(MAX_GRAPH_NEIGHBORS)
        .collect()
}

fn dedupe_impact_items(items: &mut Vec<ImpactItem>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert((item.path.clone(), item.depth, item.via.clone())));
}

fn related_tests_for_paths<'a>(
    graph: &DependencyGraph,
    paths: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    let path_set: HashSet<_> = paths.collect();
    graph
        .edges
        .iter()
        .filter(|edge| edge.matches_kind(GraphEdgeKind::Tests))
        .filter(|edge| path_set.contains(edge.to.as_str()))
        .map(|edge| edge.from.clone())
        .collect()
}

fn risk_level(affected_count: usize, related_tests: usize) -> String {
    if affected_count >= 10 || related_tests >= 5 {
        "high"
    } else if affected_count >= 3 || related_tests > 0 {
        "medium"
    } else {
        "low"
    }
    .to_string()
}

fn route_map_snapshot(snapshot: &IndexSnapshot, req: RouteMapRequest) -> RouteMapResponse {
    let mut routes = BTreeMap::<String, RouteMapEntry>::new();
    for edge in &snapshot.graph.edges {
        match edge.normalized_kind() {
            GraphEdgeKind::HandlesRoute => {
                let route = edge.from.clone();
                if !route_matches(&route, req.route.as_deref()) {
                    continue;
                }
                let entry = routes
                    .entry(route.clone())
                    .or_insert_with(|| route_entry(&route));
                entry.handlers.push(RouteEndpoint {
                    path: edge.to.clone(),
                    symbol: best_symbol_for_path(snapshot, &edge.to),
                    reason: "handles_route".to_string(),
                });
            }
            GraphEdgeKind::Fetches => {
                let route = edge.to.clone();
                if !route_matches(&route, req.route.as_deref()) {
                    continue;
                }
                let entry = routes
                    .entry(route.clone())
                    .or_insert_with(|| route_entry(&route));
                entry.consumers.push(RouteEndpoint {
                    path: edge.from.clone(),
                    symbol: best_symbol_for_path(snapshot, &edge.from),
                    reason: "fetches".to_string(),
                });
            }
            _ => {}
        }
    }
    for entry in routes.values_mut() {
        let handlers: Vec<_> = entry
            .handlers
            .iter()
            .map(|handler| handler.path.clone())
            .collect();
        for handler in handlers {
            for edge in snapshot.graph.edges.iter().filter(|edge| {
                edge.from == handler
                    && matches!(
                        edge.normalized_kind(),
                        GraphEdgeKind::Calls | GraphEdgeKind::Imports
                    )
            }) {
                if snapshot.manifest.files.contains_key(&edge.to) {
                    entry.downstream.push(RouteEndpoint {
                        path: edge.to.clone(),
                        symbol: best_symbol_for_path(snapshot, &edge.to),
                        reason: edge.normalized_kind().as_str().to_string(),
                    });
                }
            }
        }
        dedupe_route_endpoints(&mut entry.handlers);
        dedupe_route_endpoints(&mut entry.consumers);
        dedupe_route_endpoints(&mut entry.downstream);
    }
    RouteMapResponse {
        routes: routes.into_values().collect(),
    }
}

fn route_entry(route: &str) -> RouteMapEntry {
    RouteMapEntry {
        route: route.to_string(),
        kind: if route.starts_with("tool:") {
            "tool"
        } else {
            "route"
        }
        .to_string(),
        handlers: Vec::new(),
        consumers: Vec::new(),
        downstream: Vec::new(),
    }
}

fn route_matches(route: &str, filter: Option<&str>) -> bool {
    filter
        .map(|filter| route.contains(filter) || route.trim_start_matches("route:") == filter)
        .unwrap_or(true)
}

fn dedupe_route_endpoints(endpoints: &mut Vec<RouteEndpoint>) {
    let mut seen = HashSet::new();
    endpoints.retain(|endpoint| seen.insert(endpoint.path.clone()));
}

fn analyze_changes_snapshot(
    root: &Path,
    snapshot: &IndexSnapshot,
    req: ChangeAnalysisRequest,
) -> Result<ChangeAnalysisResponse> {
    let changed_files = git_changed_files(root, req.include_untracked)?;
    let hunk_ranges = git_diff_hunk_ranges(root)?;
    let changed_symbols = changed_symbols_for_ranges(snapshot, &changed_files, &hunk_ranges);
    let mut impacted = Vec::new();
    let mut related_tests = Vec::new();
    for file in &changed_files {
        let response = impact_snapshot(
            snapshot,
            ImpactRequest {
                target_path: Some(file.path.clone()),
                symbol_name: None,
                direction: ImpactDirection::Both,
                include_tests: req.include_tests,
                max_depth: Some(1),
            },
        );
        related_tests.extend(response.related_tests.clone());
        impacted.push(ChangeImpact {
            path: file.path.clone(),
            risk_level: response.risk_level,
            affected_count: response.affected.len(),
        });
    }
    related_tests.sort();
    related_tests.dedup();
    Ok(ChangeAnalysisResponse {
        suggested_checks: suggested_checks_for_files(&changed_files),
        suggested_tests: related_tests,
        changed_files,
        changed_symbols,
        impacted,
    })
}

fn git_changed_files(root: &Path, include_untracked: bool) -> Result<Vec<ChangedFile>> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.len() < 4 {
            continue;
        }
        let status = &line[..2];
        if status == "??" && !include_untracked {
            continue;
        }
        let path = line[3..]
            .split(" -> ")
            .last()
            .unwrap_or_default()
            .to_string();
        files.push(ChangedFile {
            path,
            status: status.trim().to_string(),
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn git_diff_hunk_ranges(root: &Path) -> Result<BTreeMap<String, Vec<(usize, usize)>>> {
    let mut ranges = BTreeMap::<String, Vec<(usize, usize)>>::new();
    for args in [
        ["diff", "--unified=0", "--no-ext-diff"].as_slice(),
        ["diff", "--cached", "--unified=0", "--no-ext-diff"].as_slice(),
    ] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()?;
        if !output.status.success() {
            continue;
        }
        parse_diff_hunks(&String::from_utf8_lossy(&output.stdout), &mut ranges);
    }
    Ok(ranges)
}

fn parse_diff_hunks(diff: &str, ranges: &mut BTreeMap<String, Vec<(usize, usize)>>) {
    let mut current_path = None;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_path = Some(path.to_string());
            continue;
        }
        if !line.starts_with("@@") {
            continue;
        }
        let Some(path) = current_path.clone() else {
            continue;
        };
        if let Some((start, len)) = parse_new_hunk_range(line) {
            ranges
                .entry(path)
                .or_default()
                .push((start, start + len.saturating_sub(1)));
        }
    }
}

fn parse_new_hunk_range(line: &str) -> Option<(usize, usize)> {
    let plus = line.split_whitespace().find(|part| part.starts_with('+'))?;
    let plus = plus.trim_start_matches('+');
    let mut parts = plus.split(',');
    let start = parts.next()?.parse().ok()?;
    let len = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    Some((start, len.max(1)))
}

fn changed_symbols_for_ranges(
    snapshot: &IndexSnapshot,
    files: &[ChangedFile],
    hunk_ranges: &BTreeMap<String, Vec<(usize, usize)>>,
) -> Vec<ChangedSymbol> {
    let file_set: HashSet<_> = files.iter().map(|file| file.path.as_str()).collect();
    let mut symbols = Vec::new();
    for symbol in &snapshot.symbols.symbols {
        if !file_set.contains(symbol.path.as_str()) {
            continue;
        }
        let changed = hunk_ranges
            .get(&symbol.path)
            .map(|ranges| {
                ranges.iter().any(|(start, end)| {
                    *start <= symbol.end_line.max(symbol.start_line) && *end >= symbol.start_line
                })
            })
            .unwrap_or(true);
        if changed {
            symbols.push(ChangedSymbol {
                path: symbol.path.clone(),
                name: symbol.name.clone(),
                kind: symbol.kind.clone(),
                start_line: symbol.start_line,
                end_line: symbol.end_line,
            });
        }
    }
    symbols.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    symbols
}

fn suggested_checks_for_files(files: &[ChangedFile]) -> Vec<String> {
    let mut checks = Vec::new();
    if files
        .iter()
        .any(|file| file.path.starts_with("crates/jcode-codebase-sync/"))
    {
        checks.push("cargo test -p jcode-codebase-sync".to_string());
    }
    if files
        .iter()
        .any(|file| file.path.starts_with("crates/jcode-codebase-retrieval/"))
    {
        checks.push("cargo test -p jcode-codebase-retrieval".to_string());
    }
    if files
        .iter()
        .any(|file| file.path.starts_with("src/tool/codebase"))
    {
        checks.push("cargo test -p jcode e2e_search_can_render_retrieval_trace".to_string());
    }
    if files.iter().any(|file| file.path.ends_with(".rs")) {
        checks.push("cargo check".to_string());
    }
    checks.sort();
    checks.dedup();
    checks
}

#[derive(Debug, Clone)]
struct Candidate {
    path: String,
    content_hash: String,
    source: String,
    raw_score: usize,
    score: usize,
    range: ContextRange,
    why: String,
    graph_path: Vec<String>,
    node_kind: Option<String>,
    edge_kind: Option<String>,
    rerank_reasons: Vec<String>,
}

impl Candidate {
    fn new(
        path: String,
        content_hash: String,
        source: impl Into<String>,
        score: usize,
        range: ContextRange,
        why: impl Into<String>,
    ) -> Self {
        Self {
            path,
            content_hash,
            source: source.into(),
            raw_score: score,
            score,
            range,
            why: why.into(),
            graph_path: Vec::new(),
            node_kind: None,
            edge_kind: None,
            rerank_reasons: Vec::new(),
        }
    }

    fn with_graph(
        mut self,
        graph_path: Vec<String>,
        node_kind: impl Into<String>,
        edge_kind: impl Into<String>,
    ) -> Self {
        self.graph_path = graph_path;
        self.node_kind = Some(node_kind.into());
        self.edge_kind = Some(edge_kind.into());
        self
    }

    fn token_estimate(&self) -> usize {
        estimate_tokens(&self.range.text)
    }

    fn is_virtual(&self) -> bool {
        self.source == "repo_map"
    }
}

fn estimate_tokens(text: &str) -> usize {
    (text.len() / 4).max(usize::from(!text.is_empty()))
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
        .map(|hit| {
            Candidate::new(
                hit.chunk.path,
                hit.chunk.content_hash,
                "unsaved_buffer",
                hit.score + 2_000,
                ContextRange {
                    start_line: hit.chunk.start_line,
                    end_line: hit.chunk.end_line,
                    text: hit.chunk.text,
                },
                "unsaved buffer matches current editor state",
            )
        })
        .collect()
}

fn search_exact_files(snapshot: &IndexSnapshot, req: &RetrievalRequest) -> Vec<Candidate> {
    let terms = query_terms(&req.query);
    if terms.is_empty() {
        return Vec::new();
    }
    let query = req.query.to_lowercase();
    let mut candidates = Vec::new();
    for entry in snapshot.manifest.files.values() {
        let path = entry.path.to_lowercase();
        let basename = Path::new(&entry.path)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(&entry.path)
            .to_lowercase();
        let stem = basename.split('.').next().unwrap_or(&basename);
        let path_words = path_terms(&entry.path);
        let path_word_hits = terms
            .iter()
            .filter(|term| path_words.contains(term.as_str()))
            .count();
        let matched = query.contains(&path)
            || terms.iter().any(|term| {
                term == &basename || term == stem || path.ends_with(&format!("/{term}"))
            })
            || path_word_hits >= 2;
        if !matched {
            continue;
        }
        let Some(doc) = snapshot.lexical.document(&entry.path) else {
            continue;
        };
        let lines: Vec<_> = doc.text.lines().take(MAX_RANGE_LINES).collect();
        let path_match_bonus = if path_word_hits >= 2 {
            900 + path_word_hits * 220
        } else {
            path_word_hits * 180
        };
        candidates.push(Candidate::new(
            entry.path.clone(),
            entry.content_hash.clone(),
            "file_exact",
            2_800 + score_text(&entry.path, &terms) * 120 + path_match_bonus,
            ContextRange {
                start_line: 1,
                end_line: lines.len().max(1),
                text: lines.join("\n"),
            },
            "file:exact path or basename",
        ));
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

fn search_overlay(snapshot: &IndexSnapshot, req: &RetrievalRequest) -> Vec<Candidate> {
    let terms = query_terms(&req.query);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for hit in snapshot.lexical.search(&req.query, 40) {
        let Some(chunks) = snapshot.overlay.chunks_for_path(&hit.path) else {
            continue;
        };
        for chunk in chunks {
            let score = score_text(&chunk.path, &terms) * 3 + score_text(&chunk.text, &terms);
            if score == 0 {
                continue;
            }
            candidates.push(Candidate::new(
                chunk.path.clone(),
                chunk.content_hash.clone(),
                "overlay",
                score + (hit.score * 10.0) as usize + 1_000,
                ContextRange {
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    text: chunk.text.clone(),
                },
                "saved local overlay matches current snapshot",
            ));
        }
    }
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.range.start_line.cmp(&b.range.start_line))
    });
    candidates.truncate(20);
    candidates
}

fn search_ast_chunks(snapshot: &IndexSnapshot, req: &RetrievalRequest) -> Vec<Candidate> {
    let terms = query_terms(&req.query);
    if terms.is_empty() {
        return Vec::new();
    }
    let normalized_terms: HashSet<_> = terms
        .iter()
        .map(|term| normalize_identifier(term))
        .collect();
    let lexical_paths: HashSet<_> = snapshot
        .lexical
        .search(&req.query, 160)
        .into_iter()
        .map(|hit| hit.path)
        .collect();
    let mut matched = Vec::new();
    for chunk in &snapshot.ast_chunks.chunks {
        if !snapshot.manifest.files.contains_key(&chunk.path) {
            continue;
        };
        if !lexical_paths.is_empty() && !lexical_paths.contains(&chunk.path) {
            continue;
        }
        let score = ast_chunk_score(chunk, &terms, &normalized_terms);
        if score == 0 {
            continue;
        }
        matched.push((chunk, score));
    }
    matched.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.path.cmp(&b.0.path))
            .then_with(|| a.0.start_line.cmp(&b.0.start_line))
    });
    matched.truncate(50);
    let mut candidates = Vec::new();
    for (chunk, score) in matched {
        let Some(entry) = snapshot.manifest.files.get(&chunk.path) else {
            continue;
        };
        let Some(range) = ast_chunk_range(snapshot, chunk) else {
            continue;
        };
        candidates.push(Candidate::new(
            chunk.path.clone(),
            entry.content_hash.clone(),
            "ast",
            900 + score,
            range,
            format!("ast:{}:{}", chunk.kind, chunk.name),
        ));
    }
    candidates
}

fn search_semantic(snapshot: &IndexSnapshot, req: &RetrievalRequest) -> Vec<Candidate> {
    #[cfg(feature = "embeddings")]
    {
        if let Ok(candidates) = search_embedding_semantic(snapshot, req)
            && !candidates.is_empty()
        {
            return candidates;
        }
    }
    search_deterministic_semantic(snapshot, req)
}

fn search_deterministic_semantic(
    snapshot: &IndexSnapshot,
    req: &RetrievalRequest,
) -> Vec<Candidate> {
    let hits = if snapshot.vector.is_empty() {
        ExactVectorIndex::rebuild(&snapshot.overlay).search(&req.query, 20)
    } else {
        snapshot.vector.search(&req.query, 20)
    };
    hits.into_iter()
        .filter_map(|hit| {
            let entry = snapshot.manifest.files.get(&hit.chunk.path)?;
            Some(Candidate::new(
                hit.chunk.path,
                entry.content_hash.clone(),
                "semantic_fallback",
                (hit.score * SEMANTIC_SCORE_SCALE) as usize + 700,
                ContextRange {
                    start_line: hit.chunk.start_line,
                    end_line: hit.chunk.end_line,
                    text: hit.chunk.text,
                },
                "semantic:fallback exact local vector match",
            ))
        })
        .collect()
}

#[cfg(feature = "embeddings")]
fn search_embedding_semantic(
    snapshot: &IndexSnapshot,
    req: &RetrievalRequest,
) -> Result<Vec<Candidate>> {
    let model_dir = std::env::var("JCODE_EMBEDDING_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            jcode_storage::jcode_dir()
                .unwrap_or_else(|_| PathBuf::from(".jcode"))
                .join("models")
                .join(jcode_embedding::MODEL_NAME)
        });
    if !jcode_embedding::is_model_available(&model_dir) {
        return Ok(Vec::new());
    }
    let embedder = jcode_embedding::Embedder::load_from_dir(&model_dir)?;
    let query = embedder.embed(&req.query)?;
    let mut scored = Vec::new();
    for chunk in snapshot.overlay.chunks() {
        let text = format!("{}\n{}", chunk.path, chunk.text);
        let embedding = embedder.embed(&text)?;
        let score = jcode_embedding::cosine_similarity(&query, &embedding);
        if score <= 0.2 {
            continue;
        }
        let Some(entry) = snapshot.manifest.files.get(&chunk.path) else {
            continue;
        };
        scored.push(Candidate::new(
            chunk.path.clone(),
            entry.content_hash.clone(),
            "semantic_embedding",
            (score * SEMANTIC_SCORE_SCALE) as usize + 1_000,
            ContextRange {
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                text: chunk.text.clone(),
            },
            format!("semantic:embedding cosine={score:.3}"),
        ));
    }
    scored.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    scored.truncate(20);
    Ok(scored)
}

fn search_repo_map(snapshot: &IndexSnapshot, req: &RetrievalRequest) -> Vec<Candidate> {
    let terms = term_set(&req.query);
    let wants_map = [
        "repo",
        "repository",
        "workspace",
        "package",
        "module",
        "entrypoint",
        "architecture",
        "structure",
        "test",
        "config",
    ]
    .iter()
    .any(|term| terms.contains(*term));
    if !wants_map {
        return Vec::new();
    }
    let summary = build_repo_map_summary(snapshot);
    if summary.trim().is_empty() {
        return Vec::new();
    }
    vec![Candidate::new(
        ".jcode/repo-map".to_string(),
        format!(
            "sha256:{}",
            jcode_codebase_sync::sha256_hex(summary.as_bytes())
        ),
        "repo_map",
        760,
        ContextRange {
            start_line: 1,
            end_line: summary.lines().count().max(1),
            text: summary,
        },
        "repo_map: package, entrypoint, tests, exports",
    )]
}

fn build_repo_map_summary(snapshot: &IndexSnapshot) -> String {
    let mut managers = Vec::new();
    let mut entrypoints = Vec::new();
    let mut tests = Vec::new();
    for path in snapshot.manifest.files.keys() {
        match path.as_str() {
            "Cargo.toml" => managers.push("cargo workspace".to_string()),
            "package.json" => managers.push("npm package".to_string()),
            "pnpm-workspace.yaml" => managers.push("pnpm workspace".to_string()),
            "yarn.lock" => managers.push("yarn lock".to_string()),
            "bun.lockb" | "bun.lock" => managers.push("bun lock".to_string()),
            "pyproject.toml" => managers.push("python pyproject".to_string()),
            "go.mod" => managers.push("go module".to_string()),
            "pom.xml" => managers.push("maven project".to_string()),
            "build.gradle" | "settings.gradle" => managers.push("gradle project".to_string()),
            _ => {}
        }
        if matches!(
            path.as_str(),
            "src/main.rs" | "src/lib.rs" | "main.py" | "app.py" | "src/index.ts" | "src/main.ts"
        ) {
            entrypoints.push(path.clone());
        }
        if path.contains("/test")
            || path.contains("/tests/")
            || path.ends_with("_test.rs")
            || path.ends_with(".test.ts")
            || path.ends_with("_test.py")
        {
            tests.push(path.clone());
        }
    }
    managers.sort();
    managers.dedup();
    entrypoints.sort();
    tests.sort();
    let mut exports: Vec<_> = snapshot
        .symbols
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.signature.starts_with("pub ")
                || symbol.signature.starts_with("export ")
                || matches!(symbol.kind.as_str(), "interface" | "trait")
        })
        .take(24)
        .map(|symbol| format!("{} {} ({})", symbol.kind, symbol.name, symbol.path))
        .collect();
    exports.sort();
    let mut out = String::new();
    out.push_str("Repo map\n");
    out.push_str(&format!(
        "Package managers: {}\n",
        if managers.is_empty() {
            "unknown".to_string()
        } else {
            managers.join(", ")
        }
    ));
    out.push_str(&format!(
        "Entrypoints: {}\n",
        if entrypoints.is_empty() {
            "unknown".to_string()
        } else {
            entrypoints
                .into_iter()
                .take(12)
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    out.push_str(&format!(
        "Related tests: {}\n",
        if tests.is_empty() {
            "unknown".to_string()
        } else {
            tests.into_iter().take(12).collect::<Vec<_>>().join(", ")
        }
    ));
    if !exports.is_empty() {
        out.push_str("Important exports:\n");
        for export in exports {
            out.push_str("- ");
            out.push_str(&export);
            out.push('\n');
        }
    }
    out
}

fn search_symbols(snapshot: &IndexSnapshot, req: &RetrievalRequest) -> Vec<Candidate> {
    let mut matched = Vec::new();
    let query_terms = query_terms(&req.query);
    let normalized_terms: Vec<_> = query_terms
        .iter()
        .map(|term| normalize_identifier(term))
        .filter(|term| !term.is_empty())
        .collect();
    for symbol in &snapshot.symbols.symbols {
        let normalized_name = normalize_identifier(&symbol.name);
        let exact = normalized_terms.iter().any(|term| term == &normalized_name);
        let fuzzy = !exact
            && normalized_name.len() >= 3
            && normalized_terms.iter().any(|term| {
                term.len() >= 3
                    && (normalized_name.contains(term) || term.contains(&normalized_name))
            });
        let legacy_match = !exact && !fuzzy && symbol_legacy_match(symbol, &query_terms);
        if !exact && !fuzzy && !legacy_match {
            continue;
        }
        if !snapshot.manifest.files.contains_key(&symbol.path) {
            continue;
        };
        let mut score = if exact {
            3_200
        } else if fuzzy {
            2_100
        } else {
            850
        } + symbol.confidence as usize;
        if req.active_file.as_deref() == Some(symbol.path.as_str()) {
            score += 300;
        } else if same_package_bonus(&symbol.path, req.active_file.as_deref()) > 0 {
            score += 120;
        }
        let source = if exact {
            "symbol_exact"
        } else if fuzzy {
            "symbol_fuzzy"
        } else {
            "symbol_lexical"
        };
        let why = if exact {
            format!("symbol:exact:{}", symbol.name)
        } else if fuzzy {
            format!("symbol:fuzzy:{}", symbol.name)
        } else {
            format!("symbol:name:{}", symbol.name)
        };
        matched.push((symbol, source, score, why));
    }
    matched.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| a.0.path.cmp(&b.0.path))
            .then_with(|| a.0.start_line.cmp(&b.0.start_line))
    });
    matched.truncate(30);
    let mut candidates = Vec::new();
    for (symbol, source, score, why) in matched {
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
        candidates.push(Candidate::new(
            symbol.path.clone(),
            entry.content_hash.clone(),
            source,
            score,
            range,
            why,
        ));
    }
    candidates
}

fn symbol_legacy_match(symbol: &jcode_codebase_sync::SymbolDefinition, terms: &[String]) -> bool {
    score_text(&symbol.path, terms) > 0
        || score_text(&symbol.name, terms) > 0
        || score_text(&symbol.kind, terms) > 0
        || score_text(&symbol.signature, terms) > 0
        || symbol
            .parent
            .as_deref()
            .map(|parent| score_text(parent, terms) > 0)
            .unwrap_or(false)
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
        if edge.matches_kind(GraphEdgeKind::Contains)
            || edge.matches_kind(GraphEdgeKind::BelongsToPackage)
        {
            continue;
        }
        let (seed_path, neighbor_path) = if candidate_paths.contains(edge.from.as_str()) {
            (&edge.from, &edge.to)
        } else if candidate_paths.contains(edge.to.as_str()) {
            (&edge.to, &edge.from)
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
        let confidence_bonus = graph_confidence_bonus(&edge.confidence);
        let edge_bonus = graph_edge_bonus(edge.normalized_kind());
        neighbors.push(
            Candidate::new(
                neighbor_path.clone(),
                entry.content_hash.clone(),
                "graph_neighbor",
                250 + confidence_bonus + edge_bonus,
                ContextRange {
                    start_line: 1,
                    end_line: lines.len().max(1),
                    text: lines.join("\n"),
                },
                format!(
                    "dependency graph neighbor via {} confidence={}",
                    edge.normalized_kind().as_str(),
                    edge.confidence
                ),
            )
            .with_graph(
                vec![
                    seed_path.clone(),
                    edge.normalized_kind().as_str().to_string(),
                    neighbor_path.clone(),
                ],
                graph_node_kind_label(neighbor_path, snapshot),
                edge.normalized_kind().as_str(),
            ),
        );
        if neighbors.len() >= MAX_GRAPH_NEIGHBORS {
            break;
        }
    }
    neighbors
}

fn graph_confidence_bonus(confidence: &str) -> usize {
    match confidence {
        "exact" => 220,
        "scip" => 200,
        "lsp" => 180,
        _ => 0,
    }
}

fn graph_edge_bonus(kind: GraphEdgeKind) -> usize {
    match kind {
        GraphEdgeKind::Definition | GraphEdgeKind::Reference => 160,
        GraphEdgeKind::Calls => 140,
        GraphEdgeKind::Implements | GraphEdgeKind::Overrides => 120,
        GraphEdgeKind::TypeDependency => 80,
        GraphEdgeKind::DependsOnPackage => 40,
        _ => 0,
    }
}

fn search_related_tests_with_graph(
    graph: &DependencyGraph,
    snapshot: &IndexSnapshot,
    candidates: &[Candidate],
    intent: &QueryIntent,
) -> Vec<Candidate> {
    if !matches!(
        intent,
        QueryIntent::Debug | QueryIntent::Edit | QueryIntent::Test | QueryIntent::Refactor
    ) {
        return Vec::new();
    }
    let candidate_paths: HashSet<_> = candidates
        .iter()
        .filter(|candidate| !candidate.is_virtual())
        .map(|candidate| candidate.path.as_str())
        .collect();
    let mut related = Vec::new();
    for edge in &graph.edges {
        if !edge.matches_kind(GraphEdgeKind::Tests) || !candidate_paths.contains(edge.to.as_str()) {
            continue;
        }
        let Some(entry) = snapshot.manifest.files.get(&edge.from) else {
            continue;
        };
        let Some(doc) = snapshot.lexical.document(&edge.from) else {
            continue;
        };
        let lines: Vec<_> = doc.text.lines().take(MAX_RANGE_LINES).collect();
        related.push(
            Candidate::new(
                edge.from.clone(),
                entry.content_hash.clone(),
                "related_test",
                650,
                ContextRange {
                    start_line: 1,
                    end_line: lines.len().max(1),
                    text: lines.join("\n"),
                },
                format!("related test for {}", edge.to),
            )
            .with_graph(
                vec![
                    edge.from.clone(),
                    edge.normalized_kind().as_str().to_string(),
                    edge.to.clone(),
                ],
                "test",
                edge.normalized_kind().as_str(),
            ),
        );
        if related.len() >= MAX_GRAPH_NEIGHBORS {
            break;
        }
    }
    related
}

fn bfs_distances(graph: &DependencyGraph, start: Option<&str>) -> HashMap<String, usize> {
    let mut distances = HashMap::new();
    let Some(start) = start else {
        return distances;
    };
    let mut adjacency = HashMap::<&str, Vec<&str>>::new();
    for edge in &graph.edges {
        if edge.matches_kind(GraphEdgeKind::Contains)
            || edge.matches_kind(GraphEdgeKind::BelongsToPackage)
        {
            continue;
        }
        adjacency
            .entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
        adjacency
            .entry(edge.to.as_str())
            .or_default()
            .push(edge.from.as_str());
    }
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((start.to_string(), 0usize));
    distances.insert(start.to_string(), 0);
    while let Some((current, dist)) = queue.pop_front() {
        if dist >= MAX_GRAPH_DISTANCE_DEPTH {
            continue;
        }
        let Some(neighbors) = adjacency.get(current.as_str()) else {
            continue;
        };
        for neighbor in neighbors {
            if distances.contains_key(*neighbor) {
                continue;
            }
            distances.insert((*neighbor).to_string(), dist + 1);
            queue.push_back(((*neighbor).to_string(), dist + 1));
        }
    }
    distances
}

fn ast_chunk_score(
    chunk: &jcode_codebase_sync::AstChunk,
    terms: &[String],
    normalized_terms: &HashSet<String>,
) -> usize {
    let mut score = score_text(&chunk.name, terms) * 80;
    score += score_text(&chunk.signature, terms) * 40;
    score += score_text(&chunk.kind, terms) * 20;
    score += score_text(&chunk.path, terms) * 80;
    if normalized_terms.contains(&normalize_identifier(&chunk.name)) {
        score += 1_800;
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
        let lowered_lines: Vec<_> = lines.iter().map(|line| line.to_lowercase()).collect();
        let mut best_line = None;
        let mut best_line_score = 0;
        for (index, line) in lowered_lines.iter().enumerate() {
            let score = score_lowered_text(line, &terms);
            if score > best_line_score {
                best_line_score = score;
                best_line = Some(index);
            }
        }
        let path_score = score_text(&entry.path, &terms) * 3;
        let lowered_text = text.to_lowercase();
        let doc_term_score = terms
            .iter()
            .filter(|term| lowered_text.contains(term.as_str()))
            .count()
            * 180;
        let exact_identifier_score = terms
            .iter()
            .filter(|term| term.contains('_') || term.contains('-') || term.len() >= 14)
            .filter(|term| lowered_text.contains(term.as_str()))
            .count()
            * 1_200;
        let bm25_score = (hit.score * 100.0) as usize;
        let total_score =
            bm25_score + path_score + best_line_score + doc_term_score + exact_identifier_score;
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
        let why = if path_score > 0 && best_line_score > 0 {
            "bm25:path+content".to_string()
        } else if path_score > 0 {
            "bm25:path".to_string()
        } else {
            "bm25:content".to_string()
        };
        candidates.push(Candidate::new(
            entry.path.clone(),
            entry.content_hash.clone(),
            "bm25",
            total_score,
            range,
            why,
        ));
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
        candidates.push(Candidate::new(
            entry.path.clone(),
            entry.content_hash.clone(),
            "fallback_scan",
            path_score + content_score + best_line_score,
            ContextRange {
                start_line: start + 1,
                end_line: end,
                text: lines[start..end].join("\n"),
            },
            "targeted fallback scan matched current disk",
        ));
    }
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    candidates.truncate(50);
    Ok(candidates)
}

fn candidate_matches_disk_cached(
    root: &Path,
    candidate: &Candidate,
    cache: &mut HashMap<String, Option<String>>,
) -> Result<bool> {
    let hash = if let Some(hash) = cache.get(&candidate.path) {
        hash.clone()
    } else {
        let hash = match fs::read(root.join(&candidate.path)) {
            Ok(bytes) => Some(format!(
                "sha256:{}",
                jcode_codebase_sync::sha256_hex(&bytes)
            )),
            Err(_) => None,
        };
        cache.insert(candidate.path.clone(), hash.clone());
        hash
    };
    Ok(hash.as_ref() == Some(&candidate.content_hash))
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
                candidate.rerank_reasons.extend(reasons.clone());
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
    } else if why.starts_with("symbol:") || why.starts_with("ast:") {
        weights.symbol.max(0) as usize
    } else if why.starts_with("semantic:") {
        weights.vector.max(0) as usize
    } else if why.starts_with("dependency graph neighbor") || why.starts_with("related test") {
        weights.graph_neighbor.max(0) as usize
    } else if why.starts_with("repo_map:") {
        25
    } else {
        weights.manifest.max(0) as usize
    }
}

fn term_set(text: &str) -> HashSet<String> {
    query_terms(text).into_iter().collect()
}

fn path_terms(path: &str) -> HashSet<String> {
    path.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter_map(|term| {
            let term = term.trim().to_lowercase();
            (term.len() >= 2).then_some(term)
        })
        .collect()
}

fn is_generated_or_vendor_path(path: &str) -> bool {
    path.contains("/generated/")
        || path.contains("/vendor/")
        || path.contains("/dist/")
        || path.ends_with(".min.js")
}

fn is_test_path(path: &str) -> bool {
    path.contains("/test")
        || path.contains("/tests/")
        || path.ends_with("_test.rs")
        || path.ends_with(".test.ts")
        || path.ends_with("_test.py")
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

#[cfg(test)]
fn compress_candidates(candidates: Vec<Candidate>, token_budget: usize) -> ContextPack {
    compress_candidates_with_trace(candidates, token_budget, false).0
}

fn compress_candidates_with_trace(
    candidates: Vec<Candidate>,
    token_budget: usize,
    include_trace: bool,
) -> (ContextPack, Vec<RetrievalTraceCandidate>, usize, usize) {
    let byte_budget = token_budget.saturating_mul(4);
    let mut used = 0;
    let mut file_index = BTreeMap::<String, usize>::new();
    let mut files: Vec<ContextFile> = Vec::new();
    let mut omitted = 0;
    let mut trace_candidates = Vec::new();

    for candidate in candidates {
        let mut range = candidate.range.clone();
        let mut cost = range.text.len();
        let mut omitted_reason = None;
        if used + cost > byte_budget
            && !files.is_empty()
            && let Some(compacted) =
                compact_range_for_budget(&range, byte_budget.saturating_sub(used))
        {
            range = compacted;
            cost = range.text.len();
        }
        if used + cost > byte_budget && !files.is_empty() {
            omitted += 1;
            omitted_reason = Some("token budget".to_string());
            if include_trace {
                trace_candidates.push(trace_candidate(&candidate, omitted_reason));
            }
            continue;
        }
        used += cost;
        if include_trace {
            trace_candidates.push(trace_candidate(&candidate, omitted_reason));
        }
        let token_estimate = estimate_tokens(&range.text);
        if let Some(index) = file_index.get(&candidate.path).copied() {
            files[index].ranges.push(range);
            files[index].ranges.sort_by_key(|range| range.start_line);
            files[index].score = files[index].score.max(candidate.score);
            files[index].token_estimate += token_estimate;
            if !files[index].source.contains(&candidate.source) {
                files[index].source = format!("{},{}", files[index].source, candidate.source);
            }
            continue;
        }
        file_index.insert(candidate.path.clone(), files.len());
        files.push(ContextFile {
            path: candidate.path,
            content_hash: candidate.content_hash,
            ranges: vec![range],
            why_included: candidate.why,
            score: candidate.score,
            token_estimate,
            source: candidate.source,
        });
    }

    (
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
        },
        trace_candidates,
        estimate_tokens_from_bytes(used),
        omitted,
    )
}

fn trace_candidate(
    candidate: &Candidate,
    omitted_reason: Option<String>,
) -> RetrievalTraceCandidate {
    RetrievalTraceCandidate {
        path: candidate.path.clone(),
        source: candidate.source.clone(),
        raw_score: candidate.raw_score,
        final_score: candidate.score,
        start_line: candidate.range.start_line,
        end_line: candidate.range.end_line,
        token_estimate: candidate.token_estimate(),
        reason: candidate.why.clone(),
        omitted_reason,
        graph_path: candidate.graph_path.clone(),
        node_kind: candidate
            .node_kind
            .clone()
            .or_else(|| Some(infer_node_kind(&candidate.path).to_string())),
        edge_kind: candidate.edge_kind.clone(),
        rerank_reasons: candidate.rerank_reasons.clone(),
        dedupe_key: format!(
            "{}:{}-{}:{}",
            candidate.path, candidate.range.start_line, candidate.range.end_line, candidate.source
        ),
    }
}

fn infer_node_kind(path: &str) -> &'static str {
    if path.starts_with("route:") {
        "route"
    } else if path.starts_with("tool:") {
        "tool"
    } else if is_test_path(path) {
        "test"
    } else {
        "file"
    }
}

fn compact_range_for_budget(range: &ContextRange, remaining_bytes: usize) -> Option<ContextRange> {
    if remaining_bytes == 0 {
        return None;
    }
    let signature = range
        .text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    let compacted = if !signature.is_empty() {
        format!("{signature}\n// context compressed to signature")
    } else {
        format!(
            "// context omitted: lines {}-{}",
            range.start_line, range.end_line
        )
    };
    if compacted.len() > remaining_bytes {
        return None;
    }
    Some(ContextRange {
        start_line: range.start_line,
        end_line: range.start_line,
        text: compacted,
    })
}

fn estimate_tokens_from_bytes(bytes: usize) -> usize {
    (bytes / 4).max(usize::from(bytes > 0))
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut seen = HashSet::new();
    for raw in query.split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-') {
        push_query_term(raw, &mut terms, &mut seen);
        for part in identifier_parts(raw) {
            push_query_term(&part, &mut terms, &mut seen);
        }
    }
    terms
}

fn normalize_identifier(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn identifier_parts(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for segment in value.split(['_', '-']) {
        let mut current = String::new();
        for ch in segment.chars() {
            if ch.is_ascii_uppercase() && !current.is_empty() {
                parts.push(current);
                current = String::new();
            }
            current.push(ch);
        }
        if !current.is_empty() {
            parts.push(current);
        }
    }
    parts
}

fn push_query_term(raw: &str, terms: &mut Vec<String>, seen: &mut HashSet<String>) {
    let term = raw.trim().to_lowercase();
    if term.len() >= 2 && seen.insert(term.clone()) {
        terms.push(term);
    }
}

fn score_text(text: &str, terms: &[String]) -> usize {
    let haystack = text.to_lowercase();
    score_lowered_text(&haystack, terms)
}

fn score_lowered_text(haystack: &str, terms: &[String]) -> usize {
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

pub fn record_retrieval_usage_trace(root: &Path, trace: &RetrievalUsageTrace) -> Result<()> {
    let log_dir = retrieval_usage_log_dir(root);
    fs::create_dir_all(&log_dir)?;
    let date = chrono::Utc::now().format("%Y-%m-%d");
    let path = log_dir.join(format!("retrieval-usage-{date}.jsonl"));
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", serde_json::to_string(trace)?)?;
    Ok(())
}

pub fn load_retrieval_usage_traces(path: &Path) -> Result<Vec<RetrievalUsageTrace>> {
    let text = fs::read_to_string(path)?;
    let mut traces = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        traces.push(serde_json::from_str(line)?);
    }
    Ok(traces)
}

pub fn summarize_retrieval_usage_traces(traces: &[RetrievalUsageTrace]) -> RetrievalUsageSummary {
    let mut summary = RetrievalUsageSummary {
        traces_total: traces.len(),
        ..RetrievalUsageSummary::default()
    };
    let mut last_context_paths = HashSet::<String>::new();
    let mut last_context_tokens = 0usize;
    let mut calls_since_context = 0usize;
    let mut edit_hits = 0usize;
    let mut test_hits = 0usize;
    let mut edit_distances = Vec::new();

    for trace in traces {
        match trace.event_kind.as_str() {
            "retrieval_context" => {
                summary.retrieval_context_events += 1;
                summary.context_token_estimate += trace.context_token_estimate;
                last_context_paths = trace.paths.iter().cloned().collect();
                last_context_tokens = trace.context_token_estimate;
                calls_since_context = 0;
            }
            "tool_call" => {
                summary.tool_call_events += 1;
                calls_since_context += 1;
                match trace.action_kind.as_deref() {
                    Some("read") => summary.read_events += 1,
                    Some("edit") => {
                        summary.edit_events += 1;
                        if intersects_usage_paths(&last_context_paths, &trace.paths) {
                            edit_hits += 1;
                            if summary.retrieval_to_edit_distance.is_none() {
                                edit_distances.push(calls_since_context);
                            }
                        }
                    }
                    Some("test") => {
                        summary.test_events += 1;
                        if !last_context_paths.is_empty() {
                            test_hits += 1;
                        }
                    }
                    Some("check") => summary.check_events += 1,
                    _ => {}
                }
                if intersects_usage_paths(&last_context_paths, &trace.paths) {
                    summary.context_used_token_estimate += last_context_tokens;
                }
            }
            _ => {}
        }
    }
    if summary.context_token_estimate > 0 {
        let used = summary
            .context_used_token_estimate
            .min(summary.context_token_estimate);
        summary.context_waste_after_turn_bps =
            10_000u32.saturating_sub(rate_bps(used, summary.context_token_estimate));
    }
    if summary.edit_events > 0 {
        summary.edit_hit_rate_bps = rate_bps(edit_hits, summary.edit_events);
    }
    if summary.test_events > 0 {
        summary.test_hit_rate_bps = rate_bps(test_hits, summary.test_events);
    }
    summary.retrieval_to_edit_distance = edit_distances.into_iter().min();
    summary
}

pub fn retrieval_usage_trace_for_tool(
    session_id: impl Into<String>,
    message_id: impl Into<String>,
    tool_call_id: impl Into<String>,
    tool_name: impl Into<String>,
    input: &serde_json::Value,
) -> RetrievalUsageTrace {
    let tool_name = tool_name.into();
    RetrievalUsageTrace {
        timestamp: chrono::Utc::now().to_rfc3339(),
        session_id: session_id.into(),
        message_id: message_id.into(),
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.clone(),
        event_kind: "tool_call".to_string(),
        action_kind: usage_action_kind(&tool_name, input).map(str::to_string),
        command_label: usage_command_label(&tool_name, input),
        paths: extract_usage_paths(input),
        context_token_estimate: 0,
        context_used_token_estimate: 0,
        context_waste_after_turn_bps: 0,
        edit_hit_rate_bps: 0,
        test_hit_rate_bps: 0,
        retrieval_to_edit_distance: None,
    }
}

fn intersects_usage_paths(candidates: &HashSet<String>, paths: &[String]) -> bool {
    paths.iter().any(|path| candidates.contains(path))
}

fn usage_action_kind(tool_name: &str, input: &serde_json::Value) -> Option<&'static str> {
    match tool_name {
        "read" | "read_file" | "file_read" | "grep" | "file_grep" | "glob" | "file_glob" => {
            Some("read")
        }
        "edit" | "file_edit" | "write" | "write_file" | "file_write" => Some("edit"),
        "bash" | "shell_exec" => shell_action_kind(input),
        _ => None,
    }
}

fn shell_action_kind(input: &serde_json::Value) -> Option<&'static str> {
    let command = command_text(input)?;
    let lower = command.to_ascii_lowercase();
    if lower.contains("cargo test")
        || lower.contains("npm test")
        || lower.contains("pnpm test")
        || lower.contains("yarn test")
        || lower.contains("bun test")
        || lower.contains("pytest")
        || lower.contains("go test")
    {
        Some("test")
    } else if lower.contains("cargo check")
        || lower.contains("cargo clippy")
        || lower.contains("cargo fmt")
        || lower.contains("npm run lint")
        || lower.contains("pnpm lint")
        || lower.contains("yarn lint")
        || lower.contains("go vet")
    {
        Some("check")
    } else {
        None
    }
}

fn usage_command_label(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    if !matches!(tool_name, "bash" | "shell_exec") {
        return None;
    }
    let command = command_text(input)?;
    let words: Vec<_> = command.split_whitespace().take(3).collect();
    (!words.is_empty()).then(|| words.join(" "))
}

fn command_text(input: &serde_json::Value) -> Option<&str> {
    input
        .get("cmd")
        .or_else(|| input.get("command"))
        .or_else(|| input.get("script"))
        .and_then(serde_json::Value::as_str)
}

fn retrieval_usage_log_dir(_root: &Path) -> PathBuf {
    std::env::var_os("JCODE_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".jcode")))
        .unwrap_or_else(|| PathBuf::from(".jcode"))
        .join("logs")
}

fn extract_usage_paths(input: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    collect_usage_paths(input, &mut paths);
    paths.sort();
    paths.dedup();
    paths
}

fn collect_usage_paths(value: &serde_json::Value, paths: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "path" | "file_path" | "target_path" | "active_file" | "fixture_path"
                ) && let Some(path) = value.as_str()
                {
                    paths.push(path.to_string());
                }
                collect_usage_paths(value, paths);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_usage_paths(item, paths);
            }
        }
        serde_json::Value::String(text)
            if text.contains('/')
                && (text.ends_with(".rs")
                    || text.ends_with(".ts")
                    || text.ends_with(".tsx")
                    || text.ends_with(".js")
                    || text.ends_with(".py")) =>
        {
            paths.push(text.clone());
        }
        _ => {}
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
                    include_trace: false,
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
    fn debug_query_includes_related_tests() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn login() {}\n");
        write(
            &dir.path().join("src/auth_test.rs"),
            "#[test]\nfn login_test() { login(); }\n",
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "debug login".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                    include_trace: true,
                },
            )
            .unwrap();
        assert!(
            response
                .context_pack
                .files
                .iter()
                .any(|file| file.path == "src/auth_test.rs")
        );
        assert!(
            response
                .trace
                .unwrap()
                .candidates
                .iter()
                .any(|candidate| candidate.source == "related_test")
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
                    include_trace: false,
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
                    include_trace: false,
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
                    include_trace: false,
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
                    include_trace: false,
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
                    include_trace: false,
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
                    include_trace: false,
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
                Candidate::new(
                    "src/lib.rs".to_string(),
                    "sha256:a".to_string(),
                    "test",
                    2,
                    ContextRange {
                        start_line: 10,
                        end_line: 11,
                        text: "second".to_string(),
                    },
                    "test",
                ),
                Candidate::new(
                    "src/lib.rs".to_string(),
                    "sha256:a".to_string(),
                    "test",
                    1,
                    ContextRange {
                        start_line: 1,
                        end_line: 2,
                        text: "first".to_string(),
                    },
                    "test",
                ),
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
            Candidate::new(
                "src/generated/auth.rs".to_string(),
                "sha256:a".to_string(),
                "test",
                50,
                ContextRange {
                    start_line: 1,
                    end_line: 1,
                    text: "validate password".to_string(),
                },
                "test",
            ),
            Candidate::new(
                "src/auth.rs".to_string(),
                "sha256:b".to_string(),
                "test",
                50,
                ContextRange {
                    start_line: 1,
                    end_line: 1,
                    text: "validate password".to_string(),
                },
                "test",
            ),
        ];
        rerank_candidates(
            &mut candidates,
            &RetrievalRequest {
                query: "validate password".to_string(),
                active_file: None,
                token_budget: None,
                unsaved_buffers: Vec::new(),
                include_trace: false,
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
                    include_trace: false,
                },
            )
            .unwrap();
        assert_eq!(response.context_pack.files[0].path, "src/other.rs");
    }

    #[test]
    fn exact_symbol_match_beats_content_similarity() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn validate_password() { strong_hash(); }\n",
        );
        write(
            &dir.path().join("src/docs.rs"),
            "pub fn notes() { /* validate password validate password */ }\n",
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
                    include_trace: true,
                },
            )
            .unwrap();
        assert_eq!(response.context_pack.files[0].path, "src/auth.rs");
        assert!(
            response
                .trace
                .unwrap()
                .candidates
                .iter()
                .any(|candidate| candidate.source == "symbol_exact")
        );
    }

    #[test]
    fn fuzzy_symbol_match_handles_camel_snake_query() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn validate_password_strength() {}\n",
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "validatePassword".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                    include_trace: true,
                },
            )
            .unwrap();
        assert_eq!(response.context_pack.files[0].path, "src/auth.rs");
        assert!(
            response
                .trace
                .unwrap()
                .candidates
                .iter()
                .any(|candidate| candidate.source == "symbol_fuzzy")
        );
    }

    #[test]
    fn duplicate_symbol_prefers_active_file_package() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/auth/login.rs"), "pub fn build() {}\n");
        write(
            &dir.path().join("src/billing/login.rs"),
            "pub fn build() {}\n",
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "build".to_string(),
                    active_file: Some("src/billing/view.rs".to_string()),
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                    include_trace: false,
                },
            )
            .unwrap();
        assert_eq!(response.context_pack.files[0].path, "src/billing/login.rs");
    }

    #[test]
    fn repo_map_query_returns_virtual_summary() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(
            &dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        );
        write(&dir.path().join("src/main.rs"), "fn main() {}\n");
        write(
            &dir.path().join("tests/login.rs"),
            "#[test]\nfn login() {}\n",
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "repo package entrypoint tests".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                    include_trace: true,
                },
            )
            .unwrap();
        let repo_map = response
            .context_pack
            .files
            .iter()
            .find(|file| file.source == "repo_map")
            .expect("repo map context");
        assert!(repo_map.ranges[0].text.contains("cargo workspace"));
        assert!(repo_map.ranges[0].text.contains("src/main.rs"));
    }

    #[test]
    fn impact_reports_callers_and_related_tests() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn login() {}\n");
        write(
            &dir.path().join("src/service.rs"),
            "pub fn run() { login(); }\n",
        );
        write(
            &dir.path().join("src/auth_test.rs"),
            "#[test]\nfn login_test() { login(); }\n",
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .impact(
                dir.path(),
                ImpactRequest {
                    target_path: Some("src/auth.rs".to_string()),
                    symbol_name: Some("login".to_string()),
                    direction: ImpactDirection::Upstream,
                    include_tests: true,
                    max_depth: Some(1),
                },
            )
            .unwrap();
        assert!(
            response
                .affected
                .iter()
                .any(|item| item.path == "src/service.rs")
        );
        assert!(
            response
                .related_tests
                .contains(&"src/auth_test.rs".to_string())
        );
    }

    #[test]
    fn route_map_links_fetch_to_handler() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(
            &dir.path().join("src/client.ts"),
            "export function load() { return fetch('/api/users'); }\n",
        );
        write(
            &dir.path().join("src/pages/api/users.ts"),
            "export function GET() {}\n",
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .route_map(
                dir.path(),
                RouteMapRequest {
                    route: Some("/api/users".to_string()),
                },
            )
            .unwrap();
        let route = response.routes.first().expect("route map entry");
        assert_eq!(route.route, "route:/api/users");
        assert!(
            route
                .consumers
                .iter()
                .any(|consumer| consumer.path == "src/client.ts")
        );
        assert!(
            route
                .handlers
                .iter()
                .any(|handler| handler.path == "src/pages/api/users.ts")
        );
    }

    #[test]
    fn analyze_changes_maps_diff_hunk_to_symbol() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        run_git_cmd(dir.path(), &["init"]);
        run_git_cmd(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git_cmd(dir.path(), &["config", "user.name", "Test"]);
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn login() {\n    old_login();\n}\n",
        );
        run_git_cmd(dir.path(), &["add", "."]);
        run_git_cmd(dir.path(), &["commit", "-m", "base"]);
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn login() {\n    new_login();\n}\n",
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .analyze_changes(dir.path(), ChangeAnalysisRequest::default())
            .unwrap();
        assert!(
            response
                .changed_files
                .iter()
                .any(|file| file.path == "src/auth.rs")
        );
        assert!(
            response
                .changed_symbols
                .iter()
                .any(|symbol| symbol.path == "src/auth.rs" && symbol.name == "login")
        );
        assert!(
            response
                .suggested_checks
                .iter()
                .any(|check| check == "cargo check")
        );
    }

    #[test]
    fn trace_candidate_includes_graph_metadata() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn login() {}\n");
        write(
            &dir.path().join("src/auth_test.rs"),
            "#[test]\nfn login_test() { login(); }\n",
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let response = engine
            .search(
                dir.path(),
                RetrievalRequest {
                    query: "debug login".to_string(),
                    active_file: None,
                    token_budget: None,
                    unsaved_buffers: Vec::new(),
                    include_trace: true,
                },
            )
            .unwrap();
        assert!(response.trace.unwrap().candidates.iter().any(|candidate| {
            candidate.edge_kind.as_deref() == Some("tests")
                && !candidate.graph_path.is_empty()
                && !candidate.dedupe_key.is_empty()
        }));
    }

    #[test]
    fn snapshot_with_code_intel_merges_scip_confidence_edges() {
        let dir = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write(&dir.path().join("src/lib.rs"), "pub fn login() {}\n");
        write(
            &dir.path().join(".scip.json"),
            r#"{"documents":[{"relative_path":"src/lib.rs","occurrences":[{"symbol":"local 0 login().","symbol_roles":1,"range":[0,0,0,3]}]}]}"#,
        );
        let engine = CodebaseRetrievalEngine::new(CodebaseSyncEngine::new(ManifestStore::new(
            store.path().to_path_buf(),
        )));
        let (snapshot, _) = engine.ensure_snapshot(dir.path()).unwrap();
        let snapshot = snapshot_with_code_intel(dir.path(), &snapshot);
        assert!(
            snapshot
                .graph
                .edges
                .iter()
                .any(|edge| edge.confidence == "scip"
                    && edge.matches_kind(GraphEdgeKind::Definition))
        );
    }

    #[test]
    fn retrieval_usage_trace_extracts_tool_paths() {
        let trace = retrieval_usage_trace_for_tool(
            "session",
            "message",
            "call",
            "read_file",
            &serde_json::json!({"path":"src/lib.rs","other":["crates/demo/src/main.rs"]}),
        );
        assert!(trace.paths.contains(&"src/lib.rs".to_string()));
        assert!(trace.paths.contains(&"crates/demo/src/main.rs".to_string()));
    }

    #[test]
    fn retrieval_usage_summary_reports_context_utility() {
        let traces = vec![
            RetrievalUsageTrace {
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                session_id: "session".to_string(),
                message_id: "message".to_string(),
                tool_call_id: "search".to_string(),
                tool_name: "codebase_search".to_string(),
                event_kind: "retrieval_context".to_string(),
                action_kind: Some("retrieval_context".to_string()),
                command_label: None,
                paths: vec!["src/lib.rs".to_string(), "tests/lib_test.rs".to_string()],
                context_token_estimate: 200,
                context_used_token_estimate: 0,
                context_waste_after_turn_bps: 0,
                edit_hit_rate_bps: 0,
                test_hit_rate_bps: 0,
                retrieval_to_edit_distance: None,
            },
            retrieval_usage_trace_for_tool(
                "session",
                "message",
                "edit",
                "edit",
                &serde_json::json!({"path":"src/lib.rs"}),
            ),
            retrieval_usage_trace_for_tool(
                "session",
                "message",
                "test",
                "bash",
                &serde_json::json!({"cmd":"cargo test -p demo"}),
            ),
        ];
        let summary = summarize_retrieval_usage_traces(&traces);
        assert_eq!(summary.retrieval_context_events, 1);
        assert_eq!(summary.edit_events, 1);
        assert_eq!(summary.test_events, 1);
        assert_eq!(summary.edit_hit_rate_bps, 10_000);
        assert_eq!(summary.test_hit_rate_bps, 10_000);
        assert_eq!(summary.retrieval_to_edit_distance, Some(1));
        assert_eq!(summary.context_waste_after_turn_bps, 0);
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
            report.cases_total >= 250,
            "retrieval eval fixture must contain at least 250 cases; got {}",
            report.cases_total
        );
        assert!(
            report.recall_at_5_rate_bps >= 9_500,
            "Recall@5 {} bps below 95% threshold. cases={} hits={} missing={:?}",
            report.recall_at_5_rate_bps,
            report.cases_total,
            report.recall_at_5_hits,
            report.recall_at_5_misses
        );
        assert!(
            report.recall_at_20_rate_bps >= 9_850,
            "Recall@20 {} bps below 98.5% threshold. cases={} hits={} missing={:?}",
            report.recall_at_20_rate_bps,
            report.cases_total,
            report.recall_at_20_hits,
            report.missing_expected
        );
        assert!(
            report.precision_at_5_bps >= 6_500,
            "Precision@5 {} bps below 65% threshold",
            report.precision_at_5_bps
        );
        assert!(
            report.impacted_test_recall_bps >= 9_000,
            "impacted-test recall {} bps below 90% threshold",
            report.impacted_test_recall_bps
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
                    include_trace: false,
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
                    include_trace: false,
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
                    include_trace: false,
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
                    include_trace: false,
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
                    include_trace: false,
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
                    include_trace: false,
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
                    include_trace: false,
                },
            )
            .unwrap();
        assert!(second.context_pack.files.is_empty());
    }
}
