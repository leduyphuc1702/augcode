# JCode Codebase Sync & Retrieval — Implementation Todo

> Generated: 2026-05-20
> Spec: `JCODE_CODEBASE_SYNC_AGENT_SPEC.md`
> Branch: `main`

## Current Status (as of 2026-05-20)

**Green (implemented + tested):**
- `crates/jcode-codebase-sync/` — 26 tests pass
  - IgnoreEngine (.gitignore + .jcodeignore), content hashing (SHA256), ManifestStore, Delta computation, FileWatcher, GitWatcher, LocalOverlayIndex, UnsavedBufferIndex, SymbolIndex, ExactVectorIndex (stub), DependencyGraph, SyncQueueScheduler, SyncOutbox, LocalCasSyncClient, SnapshotTokenPayload, CodebaseSyncEngine, WorkspaceWatcher, CommitLineage harvest
- `crates/jcode-codebase-retrieval/` — 15 tests pass
  - CodebaseRetrievalEngine, Candidate generation, rerank, compression, eval framework, branch-switch retrieval, unsaved buffer priority
- Tools integrated: `sync_status`, `sync_drain`
- `cargo check -p jcode` passes

**Red (not yet implemented):**
- `codebase_search` agent tool
- `read_file` agent tool with snapshot auth
- `commit_lineage_search` agent tool
- QueryPlanner
- Real lexical index (BM25/tantivy) — currently linear scan
- Real embedding model — ExactVectorIndex uses fake/random embeddings
- Quantized ANN
- Tree-sitter symbol-aware chunking
- Queue workers with backpressure metrics
- Metrics/logging pipeline

---

## Phase 1 — Agent Tools + End-to-End Validation

Goal: Agent can call retrieval engine through MCP/tool layer. Security invariants enforced.

### 1.1 `codebase_search` tool
- [ ] Create `src/tool/codebase_search.rs`
  - Accept: `query`, optional `intent`, optional `active_file`, optional `token_budget`
  - Call `CodebaseRetrievalEngine::search()`
  - Return `ContextPack` + `snapshot_id` + `freshness`
  - Wire into tool registry (`src/tool/mod.rs` or equivalent)
- [ ] Add unit test: tool returns `ContextPack` with matching file
- [ ] Add integration test: agent query → retrieval → context pack contains expected file
- [ ] **Test command:** `cargo test -p jcode codebase_search`

### 1.2 `read_file` tool with snapshot token auth
- [ ] Create `src/tool/read_file.rs`
  - Accept: `path`, `snapshot_token` (or derive from current workspace)
  - Call `CodebaseSyncEngine::read_file_authorized()`
  - Return `AuthorizedFileRead`
  - Reject unauthorized paths with clear error
- [ ] Add unit test: authorized read succeeds
- [ ] Add unit test: tampered token rejects
- [ ] Add unit test: path traversal rejects
- [ ] **Test command:** `cargo test -p jcode read_file`

### 1.3 `commit_lineage_search` tool
- [ ] Create `src/tool/commit_lineage_search.rs`
  - Accept: `query`, optional `branch`, optional `max_results`
  - Call `harvest_commit_lineage()` then filter by query terms
  - Return list of commits with `whyRelevant`
- [ ] Add unit test: search returns commits matching query terms
- [ ] **Test command:** `cargo test -p jcode commit_lineage`

### 1.4 End-to-end integration test
- [ ] Create `src/tool/codebase_sync_e2e_tests.rs` or similar
  - Scenario: create temp workspace → open → edit file → agent searches → finds new content
  - Scenario: switch branch → agent searches → finds branch-specific symbol
  - Scenario: delete file → agent searches → file not returned
  - **Test command:** `cargo test -p jcode e2e_codebase`

### 1.5 Tool registry wiring
- [ ] Register all 3 new tools in tool dispatcher
- [ ] Verify `cargo check -p jcode` still passes
- [ ] Run full workspace tests: `cargo test --workspace`

---

## Phase 2 — Retrieval Quality

Goal: Improve Recall@5/20 on real tasks. Replace linear scan with proper indexes.

### 2.1 QueryPlanner
- [ ] Create `crates/jcode-codebase-retrieval/src/query_planner.rs`
  - Parse intent: `understand` | `edit` | `debug` | `test` | `refactor` | `review`
  - Based on intent, adjust source weights (e.g., `test` intent boosts test files)
  - Extract semantic query vs lexical query vs symbol query
- [ ] Add tests for each intent → expected source weight
- [ ] **Test command:** `cargo test -p jcode-codebase-retrieval query_planner`

### 2.2 LexicalIndex (BM25)
- [ ] Add dependency: `tantivy = "0.21"` (or custom BM25) to retrieval crate
- [ ] Create `crates/jcode-codebase-retrieval/src/lexical_index.rs`
  - Index: `path` + `content` per file
  - Search: BM25 scoring
  - Update: incremental add/remove on delta
- [ ] Replace `search_manifest_files()` linear scan with LexicalIndex search
- [ ] Add test: lexical search ranks exact matches higher than partial matches
- [ ] Add benchmark: index 1000 files, query latency < 100ms
- [ ] **Test command:** `cargo test -p jcode-codebase-retrieval lexical`

### 2.3 Improved reranker scores
- [ ] `graph_distance_score(candidate, active_file)` — BFS distance in DependencyGraph
- [ ] `same_package_or_service_score(candidate, active_file)` — common path prefix
- [ ] `recency_branch_score(candidate, snapshot)` — prefer files in recent commits
- [ ] `test_config_relevance_score(candidate, req)` — boost tests/configs when intent suggests
- [ ] `commit_lineage_score(candidate, req)` — boost files touched in relevant commits
- [ ] Add test: active file proximity boosts related files
- [ ] Add test: graph neighbors rank higher than random matches
- [ ] **Test command:** `cargo test -p jcode-codebase-retrieval rerank`

### 2.4 Retrieval eval benchmark
- [ ] Create `fixtures/retrieval_eval.json` with 10-20 real JCode tasks
  - Each: `{ "query": "...", "expected_files": [...], "expected_symbols": [...] }`
- [ ] Add `RetrievalEvalReport` metrics: Recall@5, Recall@20, MRR, stale-context rate, unauthorized rate
- [ ] Add CI script to run eval and fail if Recall@5 < 60%
- [ ] **Test command:** `cargo test -p jcode-codebase-retrieval eval`

---

## Phase 3 — Backend Indexing + Chunking

Goal: Production-grade indexing with symbol-aware chunking and real embeddings.

### 3.1 Tree-sitter symbol-aware chunking
- [ ] Add dependencies: `tree-sitter = "0.22"`, language grammars (`tree-sitter-rust`, `tree-sitter-typescript`, etc.)
- [ ] Create `crates/jcode-codebase-sync/src/chunking.rs`
  - Parse file with Tree-sitter
  - Chunk by top-level symbol (fn, class, interface, etc.)
  - Preserve parent context (imports, enclosing class)
  - Fallback to line-window for unparseable files
- [ ] Replace `chunk_text()` in `lib.rs` with new chunking
- [ ] Add test: Rust fn chunks contain only one function
- [ ] Add test: TypeScript class chunks include methods
- [ ] Add test: imports preserved in chunk metadata
- [ ] **Test command:** `cargo test -p jcode-codebase-sync chunking`

### 3.2 Real embedding integration
- [ ] Add dependency: `ort = "2"` (ONNX Runtime) or remote embedding client
- [ ] Create `crates/jcode-codebase-sync/src/embedding.rs`
  - `embed_text(text: &str) -> Vec<f32>` — real model
  - Batch embedding for efficiency
  - Cache embeddings by content_hash
- [ ] Replace stub `embed_text()` in `lib.rs`
- [ ] Add test: embedding similarity correlates with semantic similarity
- [ ] Add benchmark: embed 100 chunks, latency < 500ms
- [ ] **Test command:** `cargo test -p jcode-codebase-sync embedding`

### 3.3 LexicalIndex backend (sync crate)
- [ ] Create `crates/jcode-codebase-sync/src/lexical_index.rs`
  - BM25 index per workspace
  - Incremental updates on delta
  - Persist to disk
- [ ] Add test: index file → search returns file
- [ ] Add test: delete file → search no longer returns file
- [ ] **Test command:** `cargo test -p jcode-codebase-sync lexical_index`

### 3.4 Queue workers with backpressure
- [ ] Create `crates/jcode-codebase-sync/src/workers.rs`
  - `ChunkWorker`, `EmbeddingWorker`, `LexicalWorker`, `SymbolWorker`
  - Each worker reads from queue, processes job, marks done
  - Backpressure: pause bulk queue if hot queue depth > threshold
  - Metrics: job duration, retry count, queue depth
- [ ] Add test: hot job processed before bulk job
- [ ] Add test: failed job retried with backoff
- [ ] **Test command:** `cargo test -p jcode-codebase-sync workers`

### 3.5 Metrics/logging
- [ ] Create `crates/jcode-codebase-sync/src/metrics.rs`
  - Counters: `sync.discovery.files_total`, `sync.delta.added`, `sync.delta.modified`, `sync.delta.removed`
  - Histograms: `sync.hash.bytes_per_second`, `index.job.duration_ms`
  - Gauges: `index.queue.hot.depth`
  - Retrieval: `retrieval.local_overlay_hits`, `retrieval.latency_ms`
- [ ] Integrate metrics into engine, workers, retrieval
- [ ] Add test: metric increments correctly on delta
- [ ] **Test command:** `cargo test -p jcode-codebase-sync metrics`

---

## Phase 4 — Scale Features

Goal: Handle repos with 100k+ files. Quantized ANN. Commit lineage LLM summary.

### 4.1 Quantized ANN
- [ ] Add dependency: `hnsw = "0.13"` or `usearch = "2"`
- [ ] Create `crates/jcode-codebase-sync/src/ann_index.rs`
  - Build ANN index from stable snapshot embeddings
  - Quantize vectors (e.g., scalar quantization to int8)
  - Search: ANN candidate generation → exact rerank top-k
- [ ] Add test: ANN recall >= 95% vs exact search on benchmark
- [ ] Add benchmark: search 100k vectors, latency < 50ms
- [ ] **Test command:** `cargo test -p jcode-codebase-sync ann`

### 4.2 Commit lineage LLM summarization
- [ ] Create `crates/jcode-codebase-sync/src/commit_summary.rs`
  - `summarize_commit(diff: &str) -> CommitSummary`
  - Call lightweight LLM (local or remote) with structured prompt
  - Cache summaries by commit SHA
- [ ] Add test: summary contains key files and technical terms
- [ ] **Test command:** `cargo test -p jcode-codebase-sync commit_summary`

### 4.3 Retrieval eval dashboard
- [ ] Create `scripts/retrieval_eval.rs` or binary target
  - Run eval on fixture, output JSON report
  - Track Recall@5/20 over time (store historical results)
  - Flag regressions
- [ ] Add CI workflow to run eval on PR
- [ ] **Test command:** `cargo run -p jcode-codebase-retrieval --bin eval`

---

## Testing Checklist (run before any commit)

```bash
# Fast feedback
cargo test -p jcode-codebase-sync -- --nocapture
cargo test -p jcode-codebase-retrieval -- --nocapture
cargo check -p jcode

# Full validation
cargo test --workspace
```

## Key Files

| File | Purpose |
|---|---|
| `JCODE_CODEBASE_SYNC_AGENT_SPEC.md` | Full spec |
| `crates/jcode-codebase-sync/src/lib.rs` | Sync engine (2279 lines) |
| `crates/jcode-codebase-retrieval/src/lib.rs` | Retrieval engine (1008 lines) |
| `src/tool/sync_status.rs` | sync_status tool |
| `src/tool/sync_drain.rs` | sync_drain tool |
| `CODEBASE_SYNC_TODO.md` | This file |

## Notes for Future Sessions

- **Do NOT rely on `read` tool for these files** — it may return git status instead of content. Use `cat` or `sed`.
- **Test style:** Prefer tempfile + TempDir for filesystem tests. Use `run_git_cmd` helper for git setup.
- **Embedding stub:** Current `embed_text()` in sync `lib.rs` is a stub. Replace in Phase 3.2.
- **Vector index stub:** `ExactVectorIndex` uses fake embeddings. ANN in Phase 4.1 will replace.
- **Security invariant:** `read_file` must check `token.authorize_path_hash()` before returning content.
- **Branch exactness:** ManifestStore uses `branch_key()` to separate manifests per branch. Never mix.
