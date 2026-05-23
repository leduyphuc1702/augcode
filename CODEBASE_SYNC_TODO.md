# JCode Codebase Sync & Retrieval Todo

> Updated: 2026-05-23
> Spec: `JCODE_CODEBASE_SYNC_AGENT_SPEC.md`

## Current State

**V1 now implemented in code:**
- `features.codebase_sync = true` by default.
- `JCODE_CODEBASE_SYNC_ENABLED=0|false` disables sync/injection.
- `CodebaseSyncEngine` writes persistent local index beside manifest:
  - `~/.jcode/codebase/<workspace>/<branch>/manifest.json`
  - `~/.jcode/codebase/<workspace>/<branch>/index.json`
  - `~/.jcode/codebase/<workspace>/<branch>/index-delta.jsonl`
- Persisted snapshot includes chunks, BM25-style lexical data, symbol defs, dependency graph, manifest, indexed/skipped counts.
- Retrieval reads `IndexSnapshot` on hot path.
- Retrieval no longer rebuilds overlay/symbol/vector/BM25 from disk every normal query.
- Cold path creates the index; stale/deleted candidates are disk-hash checked and can fall back to targeted local scan.
- TUI starts background codebase sync for fresh/resumed local sessions.
- Native watcher stays alive and applies deltas.
- Progress is emitted through existing background progress notice.
- `sync_status` reports phase, percent, indexed/skipped files, last error, last update.
- Provider-neutral codebase context injection runs before turns for code/edit/debug/test/refactor prompts with a 4k token budget.

**Still intentionally not V1:**
- Cloud/CAS expansion beyond local-compatible foundation.
- Tree-sitter parsing.
- Real embeddings.
- Quantized ANN.
- SQLite/Tantivy storage.
- Dedicated multi-worker queue/backpressure pipeline.

## V1 Follow-Up Hardening

- [ ] Add explicit runtime unit/integration test: temp workspace startup emits 0% then 100% progress and writes `index.json`.
- [ ] Add watcher integration test for real native edit/delete/rename path.
- [ ] Add retrieval regression test proving hot query does not rewrite `index.json`.
- [ ] Add injection test with indexed temp workspace: code prompt injects context, non-code prompt injects none.
- [ ] Add provider-path test: non-streaming, streaming mpsc, streaming broadcast all append the codebase reminder.
- [ ] Expand `fixtures/retrieval_eval.json`; gate Recall@5 and stale/unauthorized count.
- [ ] Add logs/metrics for index latency, fallback count, stale candidate count.

## V2 Retrieval Quality

- [ ] Replace heuristic symbol extraction with tree-sitter for Rust/TS/JS/Python.
- [ ] Store richer chunk metadata: symbol name, kind, parent, imports, language, generated/vendor flag.
- [ ] Improve reranker:
  - active file proximity
  - graph distance
  - same package/service
  - tests/config relevance
  - recent commit lineage
  - open/pinned files when UI exposes them
- [ ] Replace fallback full scan with true targeted scan bounded by path/name/content prefilter.
- [ ] Add eval dashboard/CLI for Recall@5, Recall@20, MRR, stale rate, unauthorized rate.

## V3 Scale

- [ ] Move JSON/JSONL to SQLite or Tantivy-backed local store if repo scale requires it.
- [ ] Add real embedding model/client with content-hash cache.
- [ ] Add ANN index with exact rerank.
- [ ] Add queue workers: chunk, lexical, symbol, embedding.
- [ ] Add backpressure/priority: hot > warm > bulk > shadow.
- [ ] Add branch/worktree garbage collection for old local indexes.

## Validation

```bash
cargo test -p jcode-codebase-sync
cargo test -p jcode-codebase-retrieval
cargo test -p jcode codebase
cargo test -p jcode codebase_context
cargo check -p jcode
```

## Key Files

| File | Purpose |
|---|---|
| `JCODE_CODEBASE_SYNC_AGENT_SPEC.md` | Full target spec |
| `crates/jcode-codebase-sync/src/lib.rs` | Sync engine, manifest, persistent index |
| `crates/jcode-codebase-retrieval/src/lib.rs` | Retrieval over persistent index |
| `src/codebase_sync_runtime.rs` | Background auto-sync runtime |
| `src/codebase_context.rs` | Smart provider-neutral context injection |
| `src/tool/sync_status.rs` | Sync status tool |
| `src/tool/codebase_search.rs` | Agent search tool |
| `src/tool/read_file.rs` | Snapshot-authorized file read |
| `src/tool/commit_lineage_search.rs` | Commit lineage search |

## Notes

- Keep V1 local-first. Do not expand cloud sync until local index + injection quality is stable.
- Keep JSON/JSONL until evidence shows it is the bottleneck.
- Do not make fake `ExactVectorIndex` central to ranking; real embeddings + ANN belong to V3.
- Security invariant: returned saved context must match current snapshot or current disk fallback.
