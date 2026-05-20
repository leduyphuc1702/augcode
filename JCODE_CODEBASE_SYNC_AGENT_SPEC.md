# JCode Codebase Sync & Retrieval Algorithm — Implementation Brief for AI Agent

> File này là một brief triển khai duy nhất để đưa cho AI agent phát triển tiếp thuật toán sync codebase cho **JCode**.  
> Mục tiêu: xây hệ thống sync/index/retrieval cho codebase có chất lượng tương đương Augment Code theo những gì Augment công khai, đồng thời cải tiến bằng **local overlay index** để đảm bảo agent luôn đọc được thay đổi mới nhất của developer.

---

## 0. Cách dùng file này

Bạn là AI agent được giao phát triển thuật toán sync codebase cho JCode.

Hãy đọc toàn bộ file này trước khi sửa code. Sau đó:

1. Inspect repository JCode hiện tại.
2. Xác định stack, entrypoints, package manager, storage hiện có, protocol agent hiện có.
3. Map các module trong spec này vào codebase JCode.
4. Lập implementation plan ngắn gọn theo phase.
5. Implement từng phase bằng code production-grade, có test.
6. Không dừng ở prototype toy. Thuật toán phải chịu được repo lớn, branch switch, rebase, force push, file delete/rename, ignore-rule change, unsaved buffer, quyền truy cập và incremental indexing.

Nếu repo JCode chưa có module tương ứng, hãy tạo module mới với tên gần nhất với kiến trúc hiện tại. Nếu JCode có naming convention khác, ưu tiên convention của JCode.

---

## 1. Mục tiêu sản phẩm

JCode cần có một **Codebase Sync & Retrieval Engine** dùng cho AI coding agent.

Engine này phải đảm bảo:

```txt
Agent luôn truy xuất đúng context của codebase mà developer đang thật sự làm việc:
- đúng workspace
- đúng branch
- đúng worktree
- đúng file version
- thấy được thay đổi vừa lưu hoặc đang mở trong editor
- không retrieve file không thuộc quyền truy cập/snapshot hiện tại
```

Mục tiêu không chỉ là “upload files và vector search”. Mục tiêu là xây một hệ thống gồm:

```txt
workspace discovery
+ ignore/filter
+ content hashing
+ manifest/snapshot
+ incremental diff
+ missing-blob upload
+ persistent index state
+ local overlay index
+ backend indexing queues
+ code-aware chunking
+ lexical/symbol/vector/graph retrieval
+ snapshot-aware authorization
+ branch/worktree exactness
+ commit-lineage retrieval
+ evals/SLO
```

---

## 2. Nền tảng từ Augment Code công khai

Không được bịa rằng ta có source private của Augment. Những điểm dưới đây là baseline công khai cần tái tạo.

### 2.1. Workspace indexing

Augment index workspace khi mở workspace, cho phép kiểm soát file được index qua `.gitignore` và `.augmentignore`; file bị match bởi các pattern này sẽ không được index, và người dùng có thể xem sync status trong Workspace Context.[^augment-workspace-indexing]

JCode cần có tương đương:

```txt
.gitignore
+ .jcodeignore
+ sync status UI/API
+ include override bằng !pattern nếu cần
```

### 2.2. Pipeline indexing công khai

Augment Context Connectors mô tả pipeline:

```txt
Discover -> Filter -> Hash -> Diff -> Index -> Save
```

Trong đó source connector list files, filter binary/large/excluded files, compute hash để detect change, diff với stored state, gửi changed files đi embedding, rồi lưu state cho incremental run tiếp theo.[^augment-context-connectors-how-it-works]

JCode phải implement đúng pipeline này.

### 2.3. Incremental state

Context Connectors track file state để:

```txt
unchanged file -> skip
modified file  -> re-index
deleted file   -> remove from index
new file       -> add to index
```

State có thể lưu ở local filesystem hoặc remote object store để run sau chỉ xử lý file thay đổi.[^augment-context-connectors-how-it-works]

JCode phải có persistent manifest/index state, không được full re-index mọi lần.

### 2.4. Per-developer real-time index

Augment nói họ duy trì real-time index riêng cho từng developer để xử lý branch switching; index sai branch có thể làm model hallucinate. Họ đặt mục tiêu update personal search index trong vài giây sau thay đổi file và kiến trúc xử lý được many thousands of files per second.[^augment-realtime-index]

JCode phải có invariant:

```txt
Không bao giờ search default branch nếu user đang ở feature branch/worktree khác.
Không bao giờ retrieve context của snapshot cũ nếu snapshot mới đã được publish.
```

### 2.5. Workload queues

Augment tách workload queue để vừa xử lý interactive user sync vừa dùng GPU cho bulk upload/shadow indexing; các queue được cân bằng để giữ personal index sync nhanh trong khi bulk jobs không starve hệ thống.[^augment-queues]

JCode cần tối thiểu:

```txt
hotQueue    = current user edit, branch switch, active task
warmQueue   = normal repo push / background sync
bulkQueue   = first index, huge repo
shadowQueue = rebuild new embedding/index version
```

### 2.6. Proof of Possession

Augment công khai cơ chế Proof of Possession: extension tính SHA256 hash cho từng file; nếu hash chưa có trong per-tenant index thì upload file; khi completion/chat, client gửi fingerprints của files được phép truy cập; retrieval chỉ xét file có fingerprint tương ứng.[^augment-proof-possession]

JCode phải có cơ chế tương đương:

```txt
candidate chunk chỉ hợp lệ nếu contentHash nằm trong snapshot/permission token của request hiện tại.
```

### 2.7. Persistent DirectContext-style API

Augment SDK có API lưu state để tránh re-upload/re-index mọi file, hỗ trợ remove/add incremental, batch upload không chờ indexing từng batch, rồi waitForIndexing một lần.[^augment-sdk-persistent]

JCode cần có API tương đương:

```ts
createIndex()
importIndexState()
exportIndexState()
addToIndex(files, { waitForIndexing?: boolean })
removeFromIndex(paths)
waitForIndexing()
search(query, snapshotToken)
```

### 2.8. Local real-time indexing

Augment MCP docs nói local server index working directory real-time, pick up local file changes immediately, no manual sync required.[^augment-mcp-local]

JCode phải ưu tiên local-first path cho active development.

### 2.9. Quantized vector search + exact fallback

Augment công khai dùng ANN/quantized vector search cho repo rất lớn: search quantized representation trước để lấy candidate, sau đó chạy full embedding similarity trên candidates. Họ cũng dùng snapshot-aware content tracking, fallback exact search khi quantized index chưa sẵn sàng, repo quá nhỏ, embedding thiếu hoặc codebase thay đổi.[^augment-quantized]

JCode nên implement theo 2 tầng:

```txt
stable remote snapshot -> quantized ANN
recent/local delta     -> exact search
final candidates       -> full-vector rerank
```

### 2.10. Commit lineage

Augment Context Lineage index recent commits trên current branch, gồm message/author/timestamp/changed files; diff được summarize bằng lightweight LLM để compact/searchable, rồi chunk/embed cạnh normal file chunks.[^augment-context-lineage]

JCode cần có commit-history index để trả lời câu hỏi “vì sao code này như vậy?” và để agent copy pattern từ commit trước.

---

## 3. Design principle bắt buộc cho JCode

### 3.1. Snapshot exactness

Mọi query phải chạy trên một snapshot cụ thể.

```ts
type SnapshotId = string;

interface Snapshot {
  tenantId: string;
  userId: string;
  repoId: string;
  workspaceId: string;
  worktreeId: string;
  branch: string;
  headSha: string | null;
  uncommittedPatchHash: string | null;
  ignoreRulesHash: string;
  manifestRoot: string;
  createdAt: string;
}
```

Nếu user switch branch từ `main` sang `feature/a`, snapshot phải đổi. Retrieval phải filter theo snapshot mới.

### 3.2. Read-your-own-writes

File vừa save, buffer đang mở, diff chưa commit phải có thể được retrieve ngay.

Không được đợi cloud index xong mới cho agent thấy thay đổi.

JCode phải có:

```txt
remote stable index
+ local overlay index
+ unsaved buffer index
```

### 3.3. Incremental by content, không by mtime

mtime có thể sai do checkout, restore, copy, build tool, clock skew. Hash content mới là nguồn truth.

Dùng:

```txt
BLAKE3 nếu ưu tiên tốc độ local
SHA256 nếu cần tương thích proof-of-possession kiểu Augment
```

Khuyến nghị:

```txt
contentHash = sha256(content)
fastHash    = blake3(content) optional local optimization
```

### 3.4. Missing blob upload

Không upload file đã có.

Protocol:

```txt
client sends contentHash list
server returns missing hashes
client uploads only missing blobs
client commits delta metadata
```

### 3.5. Security first

Không để vector index trở thành kênh leak code giữa users/tenants.

Rule:

```txt
candidate.tenantId == request.tenantId
AND candidate.contentHash IN snapshot.allowedContentHashes
```

### 3.6. Helpful context > semantically similar context

Embedding similarity không đủ. Retrieval phải rerank theo usefulness cho coding task:

```txt
symbol match
+ lexical match
+ active file proximity
+ import/call graph distance
+ same package/service
+ test/config relevance
+ recency/current branch
+ commit lineage
+ user pinned/open files
- generated/vendor/deprecated penalty
```

---

## 4. Kiến trúc mục tiêu

```txt
JCode IDE/CLI Daemon
  ├─ FileWatcher
  ├─ GitWatcher
  ├─ IgnoreEngine
  ├─ Hasher
  ├─ ManifestStore
  ├─ LocalOverlayIndex
  ├─ UnsavedBufferIndex
  └─ SyncClient

JCode Sync API
  ├─ Auth
  ├─ findMissingBlobs()
  ├─ uploadBlobs()
  ├─ commitDelta()
  ├─ issueSnapshotToken()
  └─ syncStatus()

JCode Backend Indexing
  ├─ HotQueue
  ├─ WarmQueue
  ├─ BulkQueue
  ├─ ShadowQueue
  ├─ ChunkWorker
  ├─ EmbeddingWorker
  ├─ LexicalWorker
  ├─ SymbolGraphWorker
  └─ CommitLineageWorker

Stores
  ├─ BlobStore / CAS
  ├─ ManifestStore
  ├─ SnapshotStore
  ├─ ChunkStore
  ├─ VectorStore exact
  ├─ VectorStore quantized ANN
  ├─ LexicalIndex
  ├─ SymbolIndex
  ├─ CodeGraph
  └─ CommitIndex

JCode Retrieval API
  ├─ QueryPlanner
  ├─ LocalOverlayMerger
  ├─ SnapshotFilter
  ├─ CandidateGenerator
  ├─ FullVectorReranker
  ├─ HelpfulnessReranker
  ├─ ContextCompressor
  └─ Agent/MCP Tool Layer
```

---

## 5. Module cần tạo hoặc refactor trong JCode

Tên module có thể đổi theo convention của repo.

```txt
packages/jcode-sync/
  src/discovery/
  src/ignore/
  src/hash/
  src/manifest/
  src/delta/
  src/watch/
  src/git/
  src/client/
  src/status/

packages/jcode-index/
  src/blob-store/
  src/snapshot-store/
  src/chunking/
  src/embedding/
  src/vector/
  src/lexical/
  src/symbol/
  src/graph/
  src/commit-lineage/

packages/jcode-retrieval/
  src/query-planner/
  src/candidate-generation/
  src/snapshot-filter/
  src/rerank/
  src/compress/
  src/tools/

packages/jcode-agent-tools/
  src/codebase-search-tool.ts
  src/read-file-tool.ts
  src/sync-status-tool.ts
  src/commit-lineage-tool.ts
```

Nếu JCode là monolith, hãy tạo các namespace nội bộ tương ứng thay vì package mới.

---

## 6. Data model

### 6.1. FileEntry

```ts
interface FileEntry {
  tenantId: string;
  userId: string;
  repoId: string;
  workspaceId: string;
  worktreeId: string;
  branch: string | null;
  headSha: string | null;

  path: string;
  normalizedPath: string;
  contentHash: string;
  fastHash?: string;
  sizeBytes: number;
  mtimeMs?: number;

  language?: string;
  isGenerated?: boolean;
  isBinary?: boolean;
  isLarge?: boolean;
  ignoredBy?: ".gitignore" | ".jcodeignore" | "filter" | null;

  contents?: string; // only present during upload/indexing
}
```

### 6.2. Manifest

```ts
interface Manifest {
  schemaVersion: 1;
  tenantId: string;
  userId: string;
  repoId: string;
  workspaceId: string;
  worktreeId: string;

  branch: string | null;
  headSha: string | null;
  uncommittedPatchHash: string | null;
  ignoreRulesHash: string;
  manifestRoot: string;

  files: Record<string, {
    contentHash: string;
    sizeBytes: number;
    language?: string;
    isGenerated?: boolean;
  }>;

  createdAt: string;
}
```

### 6.3. Delta

```ts
interface Delta {
  baseSnapshotId: string | null;
  nextSnapshotId: string;

  added: FileEntry[];
  modified: FileEntry[];
  removed: string[];
  renamed: Array<{ from: string; to: string; entry: FileEntry }>;

  reason:
    | "workspace-open"
    | "file-watch"
    | "manual-rescan"
    | "branch-switch"
    | "rebase"
    | "force-push"
    | "ignore-rules-changed"
    | "provider-diff-unavailable"
    | "too-many-changes";
}
```

### 6.4. Blob

```sql
CREATE TABLE blobs (
  tenant_id TEXT NOT NULL,
  content_hash TEXT NOT NULL,
  encrypted_content BYTEA NOT NULL,
  size_bytes BIGINT NOT NULL,
  mime TEXT,
  created_at TIMESTAMP NOT NULL,
  PRIMARY KEY (tenant_id, content_hash)
);
```

### 6.5. Snapshot file mapping

```sql
CREATE TABLE snapshot_files (
  tenant_id TEXT NOT NULL,
  snapshot_id TEXT NOT NULL,
  path TEXT NOT NULL,
  content_hash TEXT NOT NULL,
  language TEXT,
  size_bytes BIGINT,
  PRIMARY KEY (tenant_id, snapshot_id, path)
);
```

### 6.6. Chunk

```ts
interface CodeChunk {
  tenantId: string;
  repoId: string;
  contentHash: string;
  chunkId: string;

  path: string;
  startLine: number;
  endLine: number;

  language: string;
  symbolName?: string;
  symbolKind?: "function" | "class" | "method" | "interface" | "type" | "module" | "config" | "unknown";

  imports: string[];
  exports: string[];
  text: string;
  textHash: string;

  generatedPenalty?: number;
  deprecatedPenalty?: number;
}
```

### 6.7. Snapshot token

```ts
interface SnapshotTokenPayload {
  tenantId: string;
  userId: string;
  repoId: string;
  workspaceId: string;
  worktreeId: string;
  snapshotId: string;
  allowedContentHashes: string[];
  pathToHash: Record<string, string>;
  expiresAt: string;
}
```

Token phải được ký server-side hoặc bằng key local được server tin cậy, tùy deployment model.

---

## 7. Ignore/filter algorithm

JCode cần file `.jcodeignore` tương đương `.augmentignore`.

### 7.1. Thứ tự filter

```txt
1. Normalize path.
2. Reject unsafe path:
   - absolute path outside workspace
   - path traversal
   - symlink escape
3. Apply .jcodeignore deny patterns.
4. Apply hard filters:
   - binary
   - invalid UTF-8 for text index
   - huge file over configured max
   - secret/key-like files
   - generated lock/build/vendor output unless explicitly included
5. Apply .gitignore.
6. Apply .jcodeignore include override !pattern if JCode chooses to support this.
```

Lưu ý: thứ tự exact có thể điều chỉnh, nhưng phải có test rõ ràng.

### 7.2. Default excludes

```gitignore
.git/
.jcode/
node_modules/
dist/
build/
out/
target/
coverage/
.venv/
venv/
__pycache__/
*.min.js
*.map
*.lock
*.png
*.jpg
*.jpeg
*.gif
*.webp
*.pdf
*.zip
*.tar
*.gz
*.pem
*.key
.env
.env.*
```

Không hardcode exclude quá mạnh nếu user explicitly include.

### 7.3. Test cases

```txt
- file ignored by .gitignore -> not indexed
- file ignored by .jcodeignore -> not indexed
- file included by !pattern -> indexed if safe
- binary file -> not indexed
- huge file -> not indexed
- .env -> not indexed
- symlink outside workspace -> not indexed
```

---

## 8. Sync algorithm

### 8.1. Workspace open

```ts
async function openWorkspace(root: string): Promise<void> {
  const repo = await detectRepo(root);
  const gitState = await readGitState(root);
  const ignoreRules = await loadIgnoreRules(root);

  const previousManifest = await manifestStore.load(root);
  const scan = await discoverFilterAndHash(root, ignoreRules);

  const nextManifest = buildManifest({
    root,
    repo,
    gitState,
    ignoreRulesHash: hashIgnoreRules(ignoreRules),
    files: scan.files,
  });

  const delta = diffManifest(previousManifest, nextManifest, "workspace-open");

  await localOverlay.apply(delta);
  await syncClient.syncDelta(delta, { priority: "hot" });

  await manifestStore.save(root, nextManifest);

  startFileWatcher(root);
  startGitWatcher(root);
  startUnsavedBufferWatcher(root);
}
```

### 8.2. File watcher

```ts
const pending = new Map<string, FileEvent>();

function onFileEvent(event: FileEvent): void {
  pending.set(event.path, event);
  debounce(flushPendingFileEvents, 100);
}

async function flushPendingFileEvents(): Promise<void> {
  const events = Array.from(pending.values());
  pending.clear();

  const paths = normalizeEvents(events);
  const ignoreRules = await loadIgnoreRules(root);

  if (paths.some(isIgnoreFile)) {
    await fullRescan("ignore-rules-changed");
    return;
  }

  const partialScan = await hashChangedPaths(paths, ignoreRules);
  const oldManifest = await manifestStore.load(root);
  const nextManifest = patchManifest(oldManifest, partialScan);

  const delta = diffManifest(oldManifest, nextManifest, "file-watch");

  // Critical: update local overlay before cloud sync.
  await localOverlay.apply(delta);

  // Cloud sync can lag; retrieval still sees local overlay.
  await syncClient.syncDelta(delta, { priority: "hot" });

  await manifestStore.save(root, nextManifest);
}
```

### 8.3. Unsaved buffer watcher

Nếu JCode có IDE integration, phải index unsaved buffer.

```ts
async function onEditorBufferChanged(path: string, contents: string): Promise<void> {
  const entry = await makeFileEntryFromBuffer(path, contents);

  await unsavedBufferIndex.upsert(entry);

  retrievalRuntime.setOverlay({
    path,
    contentHash: entry.contentHash,
    source: "unsaved-buffer",
  });
}
```

Unsaved buffer không cần upload ngay, nhưng phải có mặt trong retrieval cho active task.

### 8.4. Git watcher

```ts
async function onGitStateChanged(previous: GitState, current: GitState): Promise<void> {
  const transition = await classifyGitTransition(previous, current);

  if (
    transition.type === "force-push" ||
    transition.type === "rebase" ||
    transition.type === "unknown" ||
    transition.ignoreRulesChanged
  ) {
    await fullRescan(transition.type);
    return;
  }

  const changedPaths = await gitDiffNameStatus(previous.headSha, current.headSha);

  if (changedPaths.length > config.massChangeThreshold) {
    await fullRescan("too-many-changes");
    return;
  }

  const partialScan = await hashChangedPaths(changedPaths.map(p => p.path), await loadIgnoreRules(root));
  const oldManifest = await manifestStore.load(root);
  const nextManifest = patchManifest(oldManifest, partialScan, changedPaths);

  const delta = diffManifest(oldManifest, nextManifest, "branch-switch");

  await localOverlay.apply(delta);
  await syncClient.syncDelta(delta, { priority: "hot" });
  await manifestStore.save(root, nextManifest);
}
```

### 8.5. Full rescan fallback

```ts
async function fullRescan(reason: Delta["reason"]): Promise<void> {
  const oldManifest = await manifestStore.load(root);
  const ignoreRules = await loadIgnoreRules(root);
  const scan = await discoverFilterAndHash(root, ignoreRules);
  const gitState = await readGitState(root);

  const nextManifest = buildManifest({
    root,
    repo: await detectRepo(root),
    gitState,
    ignoreRulesHash: hashIgnoreRules(ignoreRules),
    files: scan.files,
  });

  const delta = diffManifest(oldManifest, nextManifest, reason);

  await localOverlay.apply(delta);
  await syncClient.syncDelta(delta, { priority: "hot" });
  await manifestStore.save(root, nextManifest);
}
```

### 8.6. Delta rules

```ts
function diffManifest(prev: Manifest | null, next: Manifest, reason: Delta["reason"]): Delta {
  if (!prev) {
    return {
      baseSnapshotId: null,
      nextSnapshotId: snapshotId(next),
      added: Object.entries(next.files).map(toFileEntry),
      modified: [],
      removed: [],
      renamed: [],
      reason,
    };
  }

  const added: FileEntry[] = [];
  const modified: FileEntry[] = [];
  const removed: string[] = [];

  for (const [path, nextFile] of Object.entries(next.files)) {
    const prevFile = prev.files[path];
    if (!prevFile) added.push(toFileEntry([path, nextFile]));
    else if (prevFile.contentHash !== nextFile.contentHash) modified.push(toFileEntry([path, nextFile]));
  }

  for (const path of Object.keys(prev.files)) {
    if (!next.files[path]) removed.push(path);
  }

  const renamed = detectRenamesByHash(prev, next, removed, added);

  return {
    baseSnapshotId: snapshotId(prev),
    nextSnapshotId: snapshotId(next),
    added,
    modified,
    removed,
    renamed,
    reason,
  };
}
```

Rename detection by same content hash is optimization, not correctness requirement. Correctness can be remove+add.

---

## 9. Sync client protocol

### 9.1. findMissingBlobs

Request:

```json
{
  "tenantId": "t1",
  "repoId": "r1",
  "hashes": ["sha256:...", "sha256:..."]
}
```

Response:

```json
{
  "missing": ["sha256:..."],
  "alreadyPresent": ["sha256:..."]
}
```

### 9.2. uploadBlobs

Upload only missing hashes.

```ts
async function uploadMissing(delta: Delta): Promise<void> {
  const entries = [...delta.added, ...delta.modified];
  const missing = await api.findMissingBlobs(entries.map(e => e.contentHash));

  for (const batch of batchByBytes(entries.filter(e => missing.has(e.contentHash)))) {
    await api.uploadBlobs(batch.map(e => ({
      contentHash: e.contentHash,
      path: e.path,
      contents: e.contents,
    })));
  }
}
```

### 9.3. commitDelta

```ts
async function syncDelta(delta: Delta, opts: { priority: "hot" | "warm" | "bulk" }): Promise<void> {
  await uploadMissing(delta);

  await api.commitDelta({
    baseSnapshotId: delta.baseSnapshotId,
    nextSnapshotId: delta.nextSnapshotId,
    added: delta.added.map(stripContents),
    modified: delta.modified.map(stripContents),
    removed: delta.removed,
    renamed: delta.renamed,
    priority: opts.priority,
  });
}
```

Server-side:

```ts
async function commitDelta(req: CommitDeltaRequest): Promise<void> {
  await assertAuthorized(req);
  await assertAllHashesExist(req.added.concat(req.modified));

  await snapshotStore.applyDelta(req);
  await queueIndexingJobs(req);
  await issueOrUpdateSnapshotToken(req.nextSnapshotId);
}
```

---

## 10. Backend indexing

### 10.1. Queue policy

```ts
function chooseQueue(job: IndexJob): QueueName {
  if (job.priority === "hot") return "hotQueue";
  if (job.reason === "first-index" || job.fileCount > 10_000) return "bulkQueue";
  if (job.reason === "shadow-index") return "shadowQueue";
  return "warmQueue";
}
```

Operational rules:

```txt
- hotQueue must never be starved by bulkQueue.
- bulkQueue may saturate spare GPU/CPU only.
- shadowQueue can be paused under interactive load.
- Each queue has backpressure, retry, dead-letter, metrics.
```

### 10.2. Idempotency

Every indexing job must be idempotent.

```txt
jobId = hash(indexVersion + tenantId + repoId + contentHash)
```

If chunk/embedding for `(indexVersion, contentHash)` exists, skip.

### 10.3. Chunking algorithm

Do not chunk only by fixed token window.

Use hybrid code-aware chunking:

```txt
1. Parse with Tree-sitter or language parser if available.
2. Chunk by top-level symbol:
   - function
   - class
   - method
   - interface/type
   - route handler
   - config block
3. Preserve line ranges.
4. Add parent context:
   - file path
   - class name
   - enclosing function
   - imports
   - exports
5. For huge symbols, split into overlapping windows.
6. For unparseable files, fallback line-window chunking.
7. For generated/vendor files, apply penalty or exclude.
```

### 10.4. Chunk metadata

```ts
interface ChunkMetadata {
  path: string;
  language: string;
  startLine: number;
  endLine: number;
  symbolName?: string;
  symbolKind?: string;
  enclosingSymbols: string[];
  imports: string[];
  exports: string[];
  packageName?: string;
  serviceName?: string;
  testTarget?: string;
  configTarget?: string;
}
```

### 10.5. Embedding worker

```ts
async function indexBlob(contentHash: string): Promise<void> {
  if (await chunkStore.exists(indexVersion, contentHash)) return;

  const blob = await blobStore.read(contentHash);
  const chunks = await chunkCode(blob.content, blob.path);

  const embeddings = await embeddingModel.embed(chunks.map(c => ({
    text: c.text,
    metadata: c.metadata,
  })));

  await chunkStore.put(indexVersion, contentHash, chunks);
  await exactVectorStore.put(indexVersion, chunks, embeddings);
  await lexicalIndex.put(indexVersion, chunks);
  await symbolIndex.put(indexVersion, chunks);
  await graphIndex.put(indexVersion, extractGraphEdges(chunks));
}
```

### 10.6. Graph extraction

Minimum graph edges:

```txt
file -> imports file/module
symbol -> calls symbol
symbol -> defines symbol
test -> tests source symbol/file
route -> handler
handler -> service
service -> repository/model
config -> runtime component
commit -> touched file
commit -> touched symbol
```

---

## 11. Local overlay index

Đây là cải tiến chính để JCode tốt hơn baseline Augment công khai.

### 11.1. Vì sao cần local overlay

Cloud index dù nhanh vẫn có độ trễ. Developer cần agent thấy:

```txt
- file vừa save cách đây 50ms
- buffer chưa save
- local diff chưa commit
- conflict resolution đang làm
```

### 11.2. Local overlay stores

```txt
LocalOverlayIndex
  ├─ in-memory BM25/lexical index
  ├─ symbol index từ parser/LSP
  ├─ exact small-vector index nếu có local embedding
  ├─ recent file cache
  └─ unsaved buffer cache
```

### 11.3. Merge rule

```ts
async function retrieveWithOverlay(query: string, runtime: RuntimeContext): Promise<ContextPack> {
  const local = await localOverlay.search(query, runtime);
  const unsaved = await unsavedBufferIndex.search(query, runtime);
  const remote = await remoteRetrieval.search(query, runtime.snapshotToken);

  const merged = mergeDedupePreferNewest([
    ...unsaved,
    ...local,
    ...remote,
  ]);

  return compress(await helpfulnessRerank(query, runtime, merged));
}
```

Priority:

```txt
unsaved buffer > saved local overlay > remote current snapshot > remote historical/commit context
```

### 11.4. Overlay invalidation

```txt
- file saved -> move from unsaved buffer to local overlay, then sync cloud
- branch switch -> clear overlay entries not in new manifest
- file deleted -> remove overlay path
- file renamed -> remove old path, add new path
- full rescan -> rebuild overlay summary
```

---

## 12. Retrieval algorithm

### 12.1. Query input

```ts
interface RetrievalRequest {
  query: string;
  activeFile?: string;
  cursorLine?: number;
  openFiles: string[];
  recentFiles: string[];
  diagnostics?: Diagnostic[];
  testFailures?: TestFailure[];
  gitDiff?: string;
  snapshotToken: string;
  tokenBudget: number;
}
```

### 12.2. Candidate generation

```ts
async function generateCandidates(req: RetrievalRequest): Promise<Candidate[]> {
  const planned = await queryPlanner.plan(req);

  const [
    pinned,
    unsaved,
    local,
    lexical,
    symbol,
    vector,
    graph,
    commits,
  ] = await Promise.all([
    pinnedContext(req),
    unsavedBufferIndex.search(planned),
    localOverlay.search(planned),
    lexicalIndex.search(planned),
    symbolIndex.search(planned),
    semanticSearchSnapshotAware(planned, req.snapshotToken),
    graphExpansion(planned),
    commitLineageSearch(planned),
  ]);

  return dedupeCandidates([
    ...pinned,
    ...unsaved,
    ...local,
    ...lexical,
    ...symbol,
    ...vector,
    ...graph,
    ...commits,
  ]);
}
```

### 12.3. Snapshot-aware semantic search

```ts
async function semanticSearchSnapshotAware(
  planned: PlannedQuery,
  snapshotToken: string
): Promise<Candidate[]> {
  const snapshot = verifySnapshotToken(snapshotToken);
  const q = await embedQuery(planned.semanticQuery);

  const stableBase = snapshot.stableBaseIndexId;

  const stableCandidates = await quantizedIndex.isReady(stableBase)
    ? quantizedIndex.search(q, stableBase, { k: 2000 })
    : exactVectorStore.search(q, stableBase, { k: 2000 });

  const deltaCandidates = await exactVectorStore.search(q, snapshot.deltaIndexId, { k: 500 });

  const union = dedupeCandidates([...stableCandidates, ...deltaCandidates]);

  return fullVectorRerank(q, union, { k: 120 });
}
```

### 12.4. Authorization filter

```ts
function authorizeCandidate(candidate: Candidate, snapshot: SnapshotTokenPayload): boolean {
  return (
    candidate.tenantId === snapshot.tenantId &&
    snapshot.allowedContentHashes.includes(candidate.contentHash)
  );
}
```

### 12.5. Helpfulness reranker

```ts
function scoreCandidate(c: Candidate, req: RetrievalRequest): number {
  return (
    0.25 * c.semanticScore +
    0.20 * c.lexicalSymbolScore +
    0.15 * graphDistanceScore(c, req.activeFile) +
    0.10 * samePackageOrServiceScore(c, req.activeFile) +
    0.10 * recencyBranchScore(c, req.snapshotToken) +
    0.08 * testConfigRelevanceScore(c, req) +
    0.07 * openRecentPinnedScore(c, req) +
    0.05 * commitLineageScore(c, req) -
    generatedVendorDeprecatedPenalty(c)
  );
}
```

### 12.6. Context compression

Output should fit agent prompt.

```ts
interface ContextPack {
  files: Array<{
    path: string;
    ranges: Array<{ startLine: number; endLine: number; text: string }>;
    whyIncluded: string;
  }>;
  symbols: Array<{
    name: string;
    path: string;
    summary: string;
  }>;
  commits: Array<{
    sha: string;
    summary: string;
    changedFiles: string[];
    whyIncluded: string;
  }>;
  omitted: Array<{
    reason: string;
    count: number;
  }>;
}
```

Compression rules:

```txt
- include active file/current diff first
- include exact symbol definitions over random chunks
- include tests/configs when task likely needs them
- prefer line ranges over full files unless file is small
- group adjacent ranges
- remove duplicate boilerplate
- cite path and line range for every snippet
```

---

## 13. Commit lineage

### 13.1. Harvesting

```ts
async function harvestCommits(repo: Repo, currentBranch: string): Promise<CommitRecord[]> {
  const commits = await gitLog({
    branch: currentBranch,
    maxCount: config.commitLineageMaxCommits,
    since: config.commitLineageSince,
  });

  return commits.map(c => ({
    sha: c.sha,
    author: c.author,
    timestamp: c.timestamp,
    message: c.message,
    changedFiles: c.changedFiles,
  }));
}
```

### 13.2. Diff summarization

```ts
async function summarizeCommit(commit: CommitRecord): Promise<CommitSummary> {
  const diff = await gitShow(commit.sha, { maxBytes: config.maxDiffBytes });

  return llmSummarizer.summarize({
    instruction: `
Summarize this commit for code retrieval.
Return:
- primary goal
- key files touched
- key symbols/functions/classes
- migration/refactor pattern
- risky edge cases
- technical keywords
Do not include irrelevant diff lines.
`,
    commit,
    diff,
  });
}
```

### 13.3. Commit search document

```ts
interface CommitSearchDocument {
  tenantId: string;
  repoId: string;
  branch: string;
  sha: string;
  author: string;
  timestamp: string;
  message: string;
  summary: string;
  changedFiles: string[];
  touchedSymbols: string[];
  technicalTerms: string[];
  embedding: number[];
}
```

---

## 14. Public API / tools for JCode agent

### 14.1. `codebase_search`

```ts
interface CodebaseSearchInput {
  query: string;
  intent?: "understand" | "edit" | "debug" | "test" | "refactor" | "review";
  activeFile?: string;
  tokenBudget?: number;
}

interface CodebaseSearchOutput {
  contextPack: ContextPack;
  snapshotId: string;
  freshness: {
    localOverlayIncluded: boolean;
    unsavedBuffersIncluded: boolean;
    cloudIndexLagMs?: number;
  };
}
```

### 14.2. `read_file`

```ts
interface ReadFileInput {
  path: string;
  snapshotToken: string;
}

interface ReadFileOutput {
  path: string;
  contentHash: string;
  contents: string;
}
```

Authorization:

```txt
read_file must reject if path->hash is not in snapshot token.
```

### 14.3. `sync_status`

```ts
interface SyncStatusOutput {
  workspaceId: string;
  branch: string | null;
  headSha: string | null;
  currentSnapshotId: string;
  localOverlayFiles: number;
  unsavedBufferFiles: number;
  pendingUploads: number;
  pendingIndexJobs: number;
  lastSuccessfulSyncAt?: string;
  warnings: string[];
}
```

### 14.4. `commit_lineage_search`

```ts
interface CommitLineageSearchInput {
  query: string;
  branch?: string;
  maxResults?: number;
}

interface CommitLineageSearchOutput {
  commits: Array<{
    sha: string;
    timestamp: string;
    message: string;
    summary: string;
    changedFiles: string[];
    whyRelevant: string;
  }>;
}
```

---

## 15. Sync status model

JCode should expose file/folder sync status.

```txt
not_indexed
ignored
hashing
pending_upload
uploading
pending_index
indexing
indexed_local_only
indexed_cloud
stale
error
```

Example:

```ts
interface FileSyncStatus {
  path: string;
  status:
    | "ignored"
    | "hashing"
    | "pending_upload"
    | "uploading"
    | "pending_index"
    | "indexing"
    | "indexed_local_only"
    | "indexed_cloud"
    | "stale"
    | "error";
  contentHash?: string;
  lastIndexedAt?: string;
  error?: string;
}
```

---

## 16. Failure handling

### 16.1. Network down

```txt
- local overlay continues working
- deltas appended to local durable queue
- retry with exponential backoff
- sync_status shows pending upload
```

### 16.2. Backend indexing lag

```txt
- retrieval includes local overlay
- remote results marked with snapshot/version
- never mix stale remote chunk if same path has newer local overlay
```

### 16.3. Force push/rebase

```txt
- classify as non-linear history
- full rescan
- old snapshot remains for old conversations only if token valid
- new queries use new snapshot
```

### 16.4. Ignore rules changed

```txt
- full rescan
- remove now-ignored files from index/snapshot
- add newly-included files
```

### 16.5. Too many file changes

```txt
- avoid sending thousands of tiny deltas
- full rescan
- bulk or hot priority depending whether user is active
```

### 16.6. Secret detection

```txt
- do not index likely secrets
- surface warning
- allow explicit safe override only through documented config
```

---

## 17. Tests bắt buộc

### 17.1. Unit tests

```txt
IgnoreEngine
- .gitignore excludes
- .jcodeignore excludes
- !pattern includes
- binary/large/secret filters

Hasher
- stable content hash
- mtime change without content change -> unchanged
- content change without mtime change -> modified

Manifest
- add/modify/delete/rename
- manifest root stable
- ignoreRulesHash change

Delta
- null previous state -> full add
- unchanged -> empty delta
- delete -> removed path
- rename -> remove+add or rename event

SnapshotAuth
- authorized contentHash allowed
- unauthorized contentHash rejected
- cross-tenant candidate rejected

LocalOverlay
- file save immediately searchable
- unsaved buffer searchable
- branch switch invalidates stale overlay
```

### 17.2. Integration tests

```txt
- workspace open indexes repo
- modify one file -> only one file uploaded/indexed
- delete file -> removed from retrieval
- branch switch -> retrieval reflects branch-specific symbol
- rebase/force push -> full rescan fallback
- .jcodeignore change -> full rescan
- network outage -> local search works, upload retry later
- cloud lag -> local overlay wins
- read_file unauthorized path -> denied
```

### 17.3. Retrieval evals

Create benchmark from real JCode tasks.

```txt
For each task:
- prompt
- expected files
- expected symbols
- expected tests/configs
- expected historical commits if applicable
```

Metrics:

```txt
Recall@5 expected files
Recall@20 expected files
MRR expected symbol
agent task success rate
context token waste
stale-context rate
unauthorized-candidate count
```

---

## 18. SLO / Definition of Done

### 18.1. Correctness

```txt
- 100% branch/worktree exactness in tests
- 100% read-your-own-writes through local overlay
- 0 unauthorized chunks returned
- 0 deleted files retrieved after snapshot update
```

### 18.2. Latency

```txt
Local file save -> locally searchable:
- p50 < 100 ms
- p95 < 300 ms

Normal cloud sync/index:
- p95 < 2 s for small/medium changed file if backend available

Branch switch under 1,000 changed files:
- p95 < 5 s to updated snapshot + local overlay

Retrieval:
- p95 < 300 ms without LLM reranker
- p95 < 900 ms with LLM/helpfulness reranker
```

### 18.3. Scale

```txt
Initial local discovery+hash:
- at least 5,000 files/s on normal developer machine, excluding huge files

Incremental sync:
- O(changed files), not O(repo files), when watcher event is reliable

Large repo vector search:
- stable snapshot uses ANN/quantized index
- recent delta uses exact search
- fallback exact search if ANN unavailable
```

---

## 19. Phased implementation plan

### Phase 1 — Local sync core

Deliverables:

```txt
- IgnoreEngine
- File discovery
- Content hashing
- ManifestStore
- Delta computation
- FileWatcher
- GitWatcher basic
- LocalOverlayIndex lexical/symbol
- sync_status API local only
```

Acceptance:

```txt
- modifying one file updates manifest and local overlay
- deleting file removes it
- branch switch triggers rescan or delta
- tests pass
```

### Phase 2 — Remote CAS + incremental upload

Deliverables:

```txt
- findMissingBlobs
- uploadBlobs
- commitDelta
- SnapshotStore
- SnapshotToken
- durable local outbox queue
```

Acceptance:

```txt
- unchanged files are not uploaded
- retry works after network failure
- read_file enforces snapshot token
```

### Phase 3 — Backend indexing

Deliverables:

```txt
- hot/warm/bulk queues
- ChunkWorker
- EmbeddingWorker
- LexicalIndex
- SymbolIndex
- exact VectorStore
- idempotent contentHash jobs
```

Acceptance:

```txt
- changed file becomes searchable remotely
- delete removes from snapshot retrieval
- bulk indexing cannot starve hot indexing
```

### Phase 4 — Retrieval engine

Deliverables:

```txt
- QueryPlanner
- candidate generation from local/lexical/symbol/vector
- snapshot authorization filter
- helpfulness reranker
- context compressor
- codebase_search tool
```

Acceptance:

```txt
- expected files retrieved on benchmark tasks
- local overlay wins over stale remote chunk
- context pack includes path+line ranges
```

### Phase 5 — Augment-plus features

Deliverables:

```txt
- quantized ANN for stable snapshots
- exact delta overlay search
- code graph expansion
- commit lineage summarization/index/search
- retrieval eval dashboard
```

Acceptance:

```txt
- ANN result parity measured against exact search
- recent changes handled exactly even if ANN stale
- commit lineage helps tasks that need historical pattern
```

---

## 20. Agent working rules while implementing

Do:

```txt
- Inspect repo before creating files.
- Preserve existing JCode architecture.
- Add tests with every module.
- Use content hash as truth.
- Keep cloud sync optional for local dev path if JCode supports offline mode.
- Write migrations/schema changes explicitly.
- Add metrics and logs for sync/index/retrieval.
- Document every public API.
```

Do not:

```txt
- Do not rely only on mtime.
- Do not full re-index on every run.
- Do not retrieve default branch for active feature branch.
- Do not mix chunks from old snapshot without explicit historical mode.
- Do not let vector search bypass authorization.
- Do not index secrets by default.
- Do not require manual sync for normal active development.
- Do not implement embedding-only retrieval as the final solution.
```

---

## 21. Minimal code skeleton

Use this skeleton if JCode has no existing implementation.

```ts
export class JCodeSyncEngine {
  constructor(
    private readonly manifestStore: ManifestStore,
    private readonly localOverlay: LocalOverlayIndex,
    private readonly syncClient: SyncClient,
    private readonly ignoreEngine: IgnoreEngine,
    private readonly hasher: Hasher,
    private readonly git: GitAdapter,
  ) {}

  async openWorkspace(root: string): Promise<void> {
    const previous = await this.manifestStore.load(root);
    const ignoreRules = await this.ignoreEngine.load(root);
    const gitState = await this.git.readState(root);

    const files = await discoverFilterAndHash(root, ignoreRules, this.hasher);
    const next = buildManifest(root, gitState, ignoreRules, files);
    const delta = diffManifest(previous, next, "workspace-open");

    await this.localOverlay.apply(delta);
    await this.syncClient.enqueue(delta, "hot");
    await this.manifestStore.save(root, next);
  }

  async onFileChange(root: string, paths: string[]): Promise<void> {
    if (paths.some(isIgnoreFile)) {
      return this.fullRescan(root, "ignore-rules-changed");
    }

    const previous = await this.manifestStore.loadOrThrow(root);
    const ignoreRules = await this.ignoreEngine.load(root);
    const partial = await hashChangedPaths(root, paths, ignoreRules, this.hasher);

    const next = patchManifest(previous, partial);
    const delta = diffManifest(previous, next, "file-watch");

    await this.localOverlay.apply(delta);
    await this.syncClient.enqueue(delta, "hot");
    await this.manifestStore.save(root, next);
  }

  async fullRescan(root: string, reason: Delta["reason"]): Promise<void> {
    const previous = await this.manifestStore.load(root);
    const ignoreRules = await this.ignoreEngine.load(root);
    const gitState = await this.git.readState(root);
    const files = await discoverFilterAndHash(root, ignoreRules, this.hasher);

    const next = buildManifest(root, gitState, ignoreRules, files);
    const delta = diffManifest(previous, next, reason);

    await this.localOverlay.apply(delta);
    await this.syncClient.enqueue(delta, "hot");
    await this.manifestStore.save(root, next);
  }
}
```

Retrieval skeleton:

```ts
export class JCodeRetrievalEngine {
  constructor(
    private readonly localOverlay: LocalOverlayIndex,
    private readonly unsaved: UnsavedBufferIndex,
    private readonly remote: RemoteRetrievalClient,
    private readonly reranker: HelpfulnessReranker,
    private readonly compressor: ContextCompressor,
  ) {}

  async search(req: RetrievalRequest): Promise<ContextPack> {
    const [unsaved, local, remote] = await Promise.all([
      this.unsaved.search(req),
      this.localOverlay.search(req),
      this.remote.search(req),
    ]);

    const merged = mergeDedupePreferNewest([...unsaved, ...local, ...remote]);
    const authorized = filterBySnapshot(merged, req.snapshotToken);
    const ranked = await this.reranker.rank(req, authorized);

    return this.compressor.compress(ranked, req.tokenBudget);
  }
}
```

---

## 22. Metrics cần log

```txt
sync.discovery.files_total
sync.discovery.files_ignored
sync.hash.bytes_per_second
sync.delta.added
sync.delta.modified
sync.delta.removed
sync.upload.missing_blobs
sync.upload.already_present
sync.upload.bytes
sync.snapshot.created
sync.snapshot.stale_queries
index.queue.hot.depth
index.queue.bulk.depth
index.job.duration_ms
index.job.retry_count
retrieval.local_overlay_hits
retrieval.unsaved_buffer_hits
retrieval.remote_hits
retrieval.unauthorized_filtered
retrieval.stale_remote_suppressed
retrieval.latency_ms
retrieval.expected_file_recall_at_20
```

---

## 23. Security checklist

```txt
[ ] Content encrypted at rest if backend stores source.
[ ] Transport TLS.
[ ] Tenant isolation in every table/index key.
[ ] Snapshot token required for retrieval.
[ ] Candidate authorization after every retrieval source.
[ ] read_file checks path->hash in snapshot token.
[ ] Secret-like files excluded by default.
[ ] Audit log for full-file reads.
[ ] No cross-tenant vector candidate exposed in logs.
[ ] No raw code in analytics.
```

---

## 24. Final target behavior examples

### Example A — one file edit

```txt
User edits src/auth/login.ts.
Within 100-300ms:
- LocalOverlayIndex contains new chunk.
- codebase_search("login validation") can return new code.
Within backend SLO:
- blob uploaded if missing
- chunk/embedding indexed
- snapshot status becomes indexed_cloud
```

### Example B — branch switch

```txt
User switches main -> feature/new-billing.
JCode detects head/branch change.
If diff manageable:
- hash changed paths
- patch manifest
- local overlay updated
- cloud delta committed
If diff huge/non-linear:
- full rescan
Retrieval never returns symbols that only exist on main unless explicitly historical.
```

### Example C — deleted file

```txt
User deletes src/oldPayment.ts.
Manifest removes path.
Local overlay removes path.
commitDelta records removed path.
Retrieval and read_file reject it for current snapshot.
```

### Example D — cloud index stale

```txt
Remote vector still has old src/auth.ts.
Local overlay has new src/auth.ts.
Retrieval merge suppresses stale remote chunk for same path/hash mismatch.
Agent sees new local version.
```

---

## 25. Reference sources

[^augment-workspace-indexing]: Augment Docs, “Index your workspace.” Key facts: workspace upload/indexing, `.gitignore`, `.augmentignore`, sync status, include override with `!`. https://docs.augmentcode.com/setup-augment/workspace-indexing

[^augment-context-connectors-how-it-works]: Augment Docs, “Context Connectors — How It Works.” Key facts: `Discover -> Filter -> Hash -> Diff -> Index -> Save`, semantic search flow, incremental state behavior. https://docs.augmentcode.com/context-services/context-connectors/how-it-works

[^augment-realtime-index]: Augment Blog, “A real-time index for your codebase: Secure, personal, scalable.” Key facts: personal per-developer index, branch switching, update within seconds, thousands of files/second. https://www.augmentcode.com/blog/a-real-time-index-for-your-codebase-secure-personal-scalable

[^augment-queues]: Same Augment real-time index blog. Key facts: Pub/Sub queues, bulk uploads, shadow indices, keeping GPUs saturated while preserving interactive throughput. https://www.augmentcode.com/blog/a-real-time-index-for-your-codebase-secure-personal-scalable

[^augment-proof-possession]: Augment Blog, “Securing the code that writes code.” Key facts: SHA256 file fingerprints, per-tenant index, upload missing/new/modified files, request-time fingerprints, restricted retrieval. https://www.augmentcode.com/blog/securing-the-code-that-writes-code-a-look-inside-our-ai-platform

[^augment-sdk-persistent]: Augment Docs, “Context Engine SDK API Reference.” Key facts: `addToIndex`, persistent export/import state, incremental remove/add, batch upload then wait. https://docs.augmentcode.com/context-services/sdk/api-reference

[^augment-mcp-local]: Augment Docs, “Context Engine MCP.” Key facts: local server indexes working directory real-time and picks up local file changes immediately. https://docs.augmentcode.com/context-services/mcp/overview

[^augment-quantized]: Augment Blog, “How we made code search 40% faster for 100M+ line codebases using quantized vector search.” Key facts: ANN/quantization, candidate search then full embedding similarity, snapshot-aware tracking, exact fallback. https://www.augmentcode.com/blog/repo-scale-100M-line-codebase-quantized-vector-search

[^augment-context-lineage]: Augment Blog, “Context Engine. Now with full Commit history.” Key facts: index recent commits, summarize diffs with lightweight LLM, embed summaries with normal chunks, retrieve commits on demand. https://www.augmentcode.com/blog/announcing-context-lineage

[^augment-context-connectors-source-indexer]: Public source: `augmentcode/context-connectors`, `src/core/indexer.ts`. Key facts: full index if no previous state, fetch changes, fallback full index if incremental unavailable, remove deleted, add added/modified, export full/search-only state. https://raw.githubusercontent.com/augmentcode/context-connectors/main/src/core/indexer.ts

[^augment-context-connectors-source-github]: Public source: `augmentcode/context-connectors`, `src/sources/github.ts`. Key facts: full indexing via tarball, incremental via GitHub Compare API, force-push fallback, `.gitignore`/`.augmentignore`, rename as remove+add, too-many-changes fallback. https://raw.githubusercontent.com/augmentcode/context-connectors/main/src/sources/github.ts
