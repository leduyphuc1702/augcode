use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ignore::{WalkBuilder, gitignore::GitignoreBuilder};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const SCHEMA_VERSION: u32 = 1;
const MAX_TEXT_FILE_BYTES: u64 = 1_000_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub content_hash: String,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default)]
    pub is_generated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitState {
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub worktree_root: Option<String>,
    pub uncommitted_patch_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileSyncState {
    Ignored,
    Hashing,
    IndexedLocalOnly,
    Stale,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSyncStatus {
    pub path: String,
    pub status: FileSyncState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_indexed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub schema_version: u32,
    pub workspace_id: String,
    pub manifest_root: String,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub worktree_root: Option<String>,
    pub uncommitted_patch_hash: Option<String>,
    pub ignore_rules_hash: String,
    pub files: BTreeMap<String, FileEntry>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DeltaReason {
    WorkspaceOpen,
    FileWatch,
    ManualRescan,
    BranchSwitch,
    IgnoreRulesChanged,
    TooManyChanges,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenameEntry {
    pub from: String,
    pub to: String,
    pub entry: FileEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Delta {
    pub added: Vec<FileEntry>,
    pub modified: Vec<FileEntry>,
    pub removed: Vec<String>,
    pub renamed: Vec<RenameEntry>,
    pub reason: DeltaReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverlayChunk {
    pub path: String,
    pub content_hash: String,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverlayHit {
    pub chunk: OverlayChunk,
    pub score: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalOverlayIndex {
    chunks_by_path: BTreeMap<String, Vec<OverlayChunk>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnsavedBufferIndex {
    chunks_by_path: BTreeMap<String, Vec<OverlayChunk>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExactVectorHit {
    pub chunk: OverlayChunk,
    pub score: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExactVectorIndex {
    entries: Vec<(OverlayChunk, Vec<f32>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymbolDefinition {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub start_line: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymbolIndex {
    pub symbols: Vec<SymbolDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyGraph {
    pub edges: Vec<DependencyEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotTokenPayload {
    pub workspace_id: String,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub allowed_content_hashes: Vec<String>,
    pub path_to_hash: BTreeMap<String, String>,
    pub issued_at: DateTime<Utc>,
}

impl SnapshotTokenPayload {
    pub fn from_manifest(manifest: &Manifest) -> Self {
        let path_to_hash: BTreeMap<_, _> = manifest
            .files
            .iter()
            .map(|(path, entry)| (path.clone(), entry.content_hash.clone()))
            .collect();
        let allowed_content_hashes = path_to_hash.values().cloned().collect();
        Self {
            workspace_id: manifest.workspace_id.clone(),
            branch: manifest.branch.clone(),
            head_sha: manifest.head_sha.clone(),
            allowed_content_hashes,
            path_to_hash,
            issued_at: Utc::now(),
        }
    }

    pub fn authorize_path_hash(&self, path: &str, content_hash: &str) -> bool {
        self.path_to_hash
            .get(path)
            .map(|expected| expected == content_hash)
            .unwrap_or(false)
            && self.allowed_content_hashes.iter().any(|hash| hash == content_hash)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthorizedFileRead {
    pub path: String,
    pub content_hash: String,
    pub contents: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitDeltaRequest {
    pub added: Vec<FileEntry>,
    pub modified: Vec<FileEntry>,
    pub removed: Vec<String>,
    pub renamed: Vec<RenameEntry>,
    pub priority: SyncPriority,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncPriority {
    Hot,
    Warm,
    Bulk,
    Shadow,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncStatusSummary {
    pub pending_uploads: usize,
    pub blobs_total: usize,
    pub committed_deltas: usize,
    pub queue_hot: usize,
    pub queue_warm: usize,
    pub queue_bulk: usize,
    pub queue_shadow: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncQueueJob {
    pub id: String,
    pub priority: SyncPriority,
    pub content_hashes: Vec<String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub retry_after_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncQueueScheduler {
    hot: VecDeque<SyncQueueJob>,
    warm: VecDeque<SyncQueueJob>,
    bulk: VecDeque<SyncQueueJob>,
    shadow: VecDeque<SyncQueueJob>,
}

#[derive(Debug, Clone)]
pub struct SyncOutbox {
    path: PathBuf,
}

impl SyncOutbox {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn append(&self, job: &SyncQueueJob) -> Result<()> {
        jcode_storage::append_json_line_fast(&self.path, job)
    }

    pub fn load(&self) -> Result<SyncQueueScheduler> {
        let mut scheduler = SyncQueueScheduler::default();
        let Ok(text) = fs::read_to_string(&self.path) else {
            return Ok(scheduler);
        };
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            scheduler.push(serde_json::from_str(line)?);
        }
        Ok(scheduler)
    }

    pub fn clear(&self) -> Result<()> {
        if self.path.exists() {
            fs::write(&self.path, "")?;
        }
        Ok(())
    }

    pub fn status_summary(&self, blobs_total: usize, committed_deltas: usize) -> Result<SyncStatusSummary> {
        let scheduler = self.load()?;
        let (queue_hot, queue_warm, queue_bulk, queue_shadow) = scheduler.priority_depths();
        Ok(SyncStatusSummary {
            pending_uploads: scheduler.pending_len(),
            blobs_total,
            committed_deltas,
            queue_hot,
            queue_warm,
            queue_bulk,
            queue_shadow,
        })
    }

    pub fn drain_ready(&self, root: &Path, client: &dyn SyncClient) -> Result<usize> {
        let mut scheduler = self.load()?;
        let mut drained = 0;
        let mut remaining = SyncQueueScheduler::default();
        while let Some(job) = scheduler.pop_next() {
            let missing = client.find_missing_blobs(&job.content_hashes)?;
            let entries: Vec<_> = discover_filter_hash(root, &IgnoreRules::load(root)?)?
                .files
                .into_values()
                .filter(|entry| missing.iter().any(|hash| hash == &entry.content_hash))
                .collect();
            let result = client.upload_blobs(&entries, root).and_then(|_| {
                client.commit_delta(CommitDeltaRequest {
                    added: entries,
                    modified: Vec::new(),
                    removed: Vec::new(),
                    renamed: Vec::new(),
                    priority: job.priority,
                })
            });
            match result {
                Ok(()) => drained += 1,
                Err(_) => remaining.requeue_failed(job),
            }
        }
        remaining.extend_not_ready(scheduler);
        self.clear()?;
        for job in remaining.into_jobs() {
            self.append(&job)?;
        }
        Ok(drained)
    }
}

impl SyncQueueScheduler {
    pub fn push(&mut self, job: SyncQueueJob) {
        match job.priority {
            SyncPriority::Hot => self.hot.push_back(job),
            SyncPriority::Warm => self.warm.push_back(job),
            SyncPriority::Bulk => self.bulk.push_back(job),
            SyncPriority::Shadow => self.shadow.push_back(job),
        }
    }

    pub fn pop_next(&mut self) -> Option<SyncQueueJob> {
        pop_ready(&mut self.hot)
            .or_else(|| pop_ready(&mut self.warm))
            .or_else(|| pop_ready(&mut self.bulk))
            .or_else(|| pop_ready(&mut self.shadow))
    }

    pub fn requeue_failed(&mut self, mut job: SyncQueueJob) {
        job.attempts += 1;
        job.retry_after_ms = retry_delay_ms(job.attempts);
        self.push(job);
    }

    pub fn pending_len(&self) -> usize {
        self.hot.len() + self.warm.len() + self.bulk.len() + self.shadow.len()
    }

    pub fn priority_depths(&self) -> (usize, usize, usize, usize) {
        (self.hot.len(), self.warm.len(), self.bulk.len(), self.shadow.len())
    }

    fn extend_not_ready(&mut self, scheduler: SyncQueueScheduler) {
        for job in scheduler.into_jobs().into_iter().filter(|job| job.retry_after_ms > 0) {
            self.push(job);
        }
    }

    fn into_jobs(self) -> Vec<SyncQueueJob> {
        self.hot
            .into_iter()
            .chain(self.warm)
            .chain(self.bulk)
            .chain(self.shadow)
            .collect()
    }
}

pub trait SyncClient {
    fn find_missing_blobs(&self, hashes: &[String]) -> Result<Vec<String>>;
    fn upload_blobs(&self, entries: &[FileEntry], root: &Path) -> Result<()>;
    fn commit_delta(&self, req: CommitDeltaRequest) -> Result<()>;
    fn sync_status(&self) -> Result<SyncStatusSummary>;
}

#[derive(Debug, Clone)]
pub struct LocalCasSyncClient {
    root: PathBuf,
}

impl LocalCasSyncClient {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.root.join("blobs").join(sanitize_hash_for_path(hash))
    }

    fn delta_log_path(&self) -> PathBuf {
        self.root.join("deltas.jsonl")
    }
}

impl SyncClient for LocalCasSyncClient {
    fn find_missing_blobs(&self, hashes: &[String]) -> Result<Vec<String>> {
        Ok(hashes
            .iter()
            .filter(|hash| !self.blob_path(hash).exists())
            .cloned()
            .collect())
    }

    fn upload_blobs(&self, entries: &[FileEntry], root: &Path) -> Result<()> {
        for entry in entries {
            let destination = self.blob_path(&entry.content_hash);
            if destination.exists() {
                continue;
            }
            let contents = fs::read(root.join(&entry.path))?;
            if let Some(parent) = destination.parent() {
                jcode_storage::ensure_dir(parent)?;
            }
            fs::write(destination, contents)?;
        }
        Ok(())
    }

    fn commit_delta(&self, req: CommitDeltaRequest) -> Result<()> {
        jcode_storage::append_json_line_fast(&self.delta_log_path(), &req)
    }

    fn sync_status(&self) -> Result<SyncStatusSummary> {
        let blobs_total = fs::read_dir(self.root.join("blobs"))
            .map(|entries| entries.filter_map(|entry| entry.ok()).count())
            .unwrap_or(0);
        let committed_deltas = fs::read_to_string(self.delta_log_path())
            .map(|text| text.lines().count())
            .unwrap_or(0);
        Ok(SyncStatusSummary {
            pending_uploads: 0,
            blobs_total,
            committed_deltas,
            queue_hot: 0,
            queue_warm: 0,
            queue_bulk: 0,
            queue_shadow: 0,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitSearchDocument {
    pub branch: Option<String>,
    pub sha: String,
    pub timestamp: Option<String>,
    pub message: String,
    pub changed_files: Vec<String>,
    pub technical_terms: Vec<String>,
}

pub fn harvest_commit_lineage(root: &Path, max_count: usize) -> Result<Vec<CommitSearchDocument>> {
    let git_state = read_git_state(root);
    let output = std::process::Command::new("git")
        .args(["log", &format!("-{}", max_count), "--name-only", "--format=%H%x1f%aI%x1f%s"])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8(output.stdout)?;
    let mut docs = Vec::new();
    let mut current: Option<CommitSearchDocument> = None;
    for line in text.lines() {
        if line.contains('\u{1f}') {
            if let Some(doc) = current.take() {
                docs.push(doc);
            }
            let mut parts = line.split('\u{1f}');
            current = Some(CommitSearchDocument {
                branch: git_state.branch.clone(),
                sha: parts.next().unwrap_or_default().to_string(),
                timestamp: parts.next().map(ToOwned::to_owned),
                message: parts.next().unwrap_or_default().to_string(),
                changed_files: Vec::new(),
                technical_terms: Vec::new(),
            });
            continue;
        }
        if let Some(doc) = current.as_mut() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                doc.changed_files.push(trimmed.to_string());
            }
        }
    }
    if let Some(doc) = current {
        docs.push(doc);
    }
    for doc in &mut docs {
        doc.technical_terms = commit_terms(doc);
    }
    Ok(docs)
}

fn commit_terms(doc: &CommitSearchDocument) -> Vec<String> {
    let mut terms = BTreeMap::<String, ()>::new();
    for token in doc
        .message
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
    {
        if token.len() >= 3 {
            terms.insert(token.to_lowercase(), ());
        }
    }
    for path in &doc.changed_files {
        for part in path.split(['/', '.', '-', '_']) {
            if part.len() >= 3 {
                terms.insert(part.to_lowercase(), ());
            }
        }
    }
    terms.into_keys().collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceStatus {
    pub workspace_id: String,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub worktree_root: Option<String>,
    pub files_total: usize,
    pub last_manifest_at: DateTime<Utc>,
    pub file_statuses: Vec<FileSyncStatus>,
    pub warnings: Vec<String>,
}

impl DependencyGraph {
    pub fn rebuild(root: &Path, manifest: &Manifest) -> Result<Self> {
        let mut edges = Vec::new();
        let paths: HashSet<_> = manifest.files.keys().cloned().collect();
        for path in manifest.files.keys() {
            if let Some(source) = test_source_path(path) {
                if paths.contains(&source) {
                    edges.push(DependencyEdge {
                        from: path.clone(),
                        to: source,
                        kind: "test_source".to_string(),
                    });
                }
            }
            let text = fs::read_to_string(root.join(path))?;
            for target in extract_import_targets(path, &text, &paths) {
                edges.push(DependencyEdge {
                    from: path.clone(),
                    to: target,
                    kind: "import".to_string(),
                });
            }
        }
        Ok(Self { edges })
    }
}

impl SymbolIndex {
    pub fn rebuild(root: &Path, manifest: &Manifest) -> Result<Self> {
        let mut symbols = Vec::new();
        for entry in manifest.files.values() {
            let text = fs::read_to_string(root.join(&entry.path))?;
            symbols.extend(extract_symbols(entry, &text));
        }
        Ok(Self { symbols })
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<SymbolDefinition> {
        let terms = query_terms(query);
        let mut hits: Vec<_> = self
            .symbols
            .iter()
            .filter(|symbol| score_text(&format!("{} {} {}", symbol.path, symbol.name, symbol.kind), &terms) > 0)
            .cloned()
            .collect();
        hits.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.start_line.cmp(&b.start_line)));
        hits.truncate(limit);
        hits
    }
}

impl ExactVectorIndex {
    pub fn rebuild(overlay: &LocalOverlayIndex) -> Self {
        let entries = overlay
            .chunks_by_path
            .values()
            .flat_map(|chunks| chunks.iter())
            .map(|chunk| (chunk.clone(), embed_text(&format!("{}\n{}", chunk.path, chunk.text))))
            .collect();
        Self { entries }
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<ExactVectorHit> {
        let query_vector = embed_text(query);
        let mut hits: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(chunk, vector)| {
                let score = cosine_similarity(&query_vector, vector);
                (score > 0.0).then(|| ExactVectorHit {
                    chunk: chunk.clone(),
                    score,
                })
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.chunk.path.cmp(&b.chunk.path))
        });
        hits.truncate(limit);
        hits
    }
}

impl UnsavedBufferIndex {
    pub fn upsert(&mut self, path: impl Into<String>, contents: impl AsRef<str>) {
        let path = path.into();
        let text = contents.as_ref();
        let entry = FileEntry {
            path: path.clone(),
            content_hash: format!("sha256:{}", sha256_hex(text.as_bytes())),
            size_bytes: text.len() as u64,
            language: language_for_path(Path::new(&path)),
            is_generated: is_generated_path(Path::new(&path)),
        };
        self.chunks_by_path.insert(path, chunk_text(&entry, text));
    }

    pub fn remove(&mut self, path: &str) {
        self.chunks_by_path.remove(path);
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<OverlayHit> {
        search_chunks(&self.chunks_by_path, query, limit)
    }
}

impl LocalOverlayIndex {
    pub fn apply_delta(&mut self, root: &Path, delta: &Delta) -> Result<()> {
        for path in &delta.removed {
            self.remove(path);
        }
        for rename in &delta.renamed {
            self.remove(&rename.from);
        }
        for entry in delta.added.iter().chain(delta.modified.iter()) {
            self.upsert_file(root, entry)?;
        }
        for rename in &delta.renamed {
            self.upsert_file(root, &rename.entry)?;
        }
        Ok(())
    }

    pub fn rebuild(root: &Path, manifest: &Manifest) -> Result<Self> {
        let mut overlay = Self::default();
        for entry in manifest.files.values() {
            overlay.upsert_file(root, entry)?;
        }
        Ok(overlay)
    }

    pub fn upsert_file(&mut self, root: &Path, entry: &FileEntry) -> Result<()> {
        let text = fs::read_to_string(root.join(&entry.path))?;
        self.chunks_by_path
            .insert(entry.path.clone(), chunk_text(entry, &text));
        Ok(())
    }

    pub fn remove(&mut self, path: &str) {
        self.chunks_by_path.remove(path);
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<OverlayHit> {
        search_chunks(&self.chunks_by_path, query, limit)
    }
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub files: BTreeMap<String, FileEntry>,
    pub ignored_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct IgnoreRules {
    jcodeignore: Option<ignore::gitignore::Gitignore>,
    hash: String,
}

impl IgnoreRules {
    pub fn load(root: &Path) -> Result<Self> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let path = root.join(".jcodeignore");
        let mut contents = String::new();
        let jcodeignore = if path.exists() {
            contents = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            let mut builder = GitignoreBuilder::new(&root);
            for line in contents.lines() {
                builder.add_line(Some(path.clone()), line)?;
            }
            Some(builder.build()?)
        } else {
            None
        };

        Ok(Self {
            jcodeignore,
            hash: sha256_hex(contents.as_bytes()),
        })
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    fn is_ignored(&self, relative: &Path, is_dir: bool) -> bool {
        self.jcodeignore
            .as_ref()
            .map(|rules| rules.matched(relative, is_dir).is_ignore())
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default)]
pub struct PendingFileEvents {
    paths: BTreeMap<String, PathBuf>,
}

impl PendingFileEvents {
    pub fn push(&mut self, path: PathBuf) {
        self.paths.insert(path.to_string_lossy().into_owned(), path);
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn drain(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.paths).into_values().collect()
    }

    pub fn flush(
        &mut self,
        engine: &CodebaseSyncEngine,
        root: &Path,
    ) -> Result<Option<(Manifest, Delta)>> {
        if self.is_empty() {
            return Ok(None);
        }
        let paths = self.drain();
        engine.on_file_change(root, &paths).map(Some)
    }
}

pub struct WorkspaceWatcher {
    event_tx: mpsc::Sender<PathBuf>,
    stop_tx: mpsc::Sender<()>,
    handle: Option<thread::JoinHandle<()>>,
    _native_watcher: Option<RecommendedWatcher>,
}

impl WorkspaceWatcher {
    pub fn push_path(&self, path: PathBuf) -> Result<()> {
        self.event_tx
            .send(path)
            .map_err(|err| anyhow::anyhow!("send file event: {}", err))
    }

    pub fn stop(mut self) -> Result<()> {
        let _ = self.stop_tx.send(());
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("workspace watcher thread panicked"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct CodebaseSyncEngine {
    store: ManifestStore,
}

impl CodebaseSyncEngine {
    pub fn new(store: ManifestStore) -> Self {
        Self { store }
    }

    pub fn open_workspace(&self, root: &Path) -> Result<(Manifest, Delta)> {
        self.rescan(root, DeltaReason::WorkspaceOpen)
    }

    pub fn on_file_change(&self, root: &Path, paths: &[PathBuf]) -> Result<(Manifest, Delta)> {
        if paths.iter().any(|path| is_ignore_file(path)) {
            return self.rescan(root, DeltaReason::IgnoreRulesChanged);
        }
        let previous = match self.store.load(root)? {
            Some(previous) => previous,
            None => return self.rescan(root, DeltaReason::WorkspaceOpen),
        };
        let rules = IgnoreRules::load(root)?;
        let partial = hash_changed_paths(root, paths, &rules)?;
        let next = patch_manifest(&previous, partial, root, &rules)?;
        let delta = diff_manifest(Some(&previous), &next, DeltaReason::FileWatch);
        self.store.save(root, &next)?;
        self.store.save_status(root, &status_from_manifest(&next, Vec::new()))?;
        Ok((next, delta))
    }

    pub fn rescan(&self, root: &Path, reason: DeltaReason) -> Result<(Manifest, Delta)> {
        let previous = self.store.load(root)?;
        let rules = IgnoreRules::load(root)?;
        let scan = discover_filter_hash(root, &rules)?;
        let warnings = scan.warnings.clone();
        let manifest = build_manifest(root, &rules, scan.files)?;
        let delta = diff_manifest(previous.as_ref(), &manifest, reason);
        self.store.save(root, &manifest)?;
        self.store.save_status(root, &status_from_manifest(&manifest, warnings))?;
        Ok((manifest, delta))
    }

    pub fn status(&self, root: &Path) -> Result<Option<WorkspaceStatus>> {
        if let Some(status) = self.store.load_status(root)? {
            return Ok(Some(status));
        }
        let Some(manifest) = self.store.load(root)? else {
            return Ok(None);
        };
        Ok(Some(status_from_manifest(&manifest, Vec::new())))
    }

    pub fn snapshot_token(&self, root: &Path) -> Result<Option<SnapshotTokenPayload>> {
        Ok(self.store.load(root)?.as_ref().map(SnapshotTokenPayload::from_manifest))
    }

    pub fn read_file_authorized(
        &self,
        root: &Path,
        path: &str,
        token: &SnapshotTokenPayload,
    ) -> Result<AuthorizedFileRead> {
        let manifest = self
            .store
            .load(root)?
            .ok_or_else(|| anyhow::anyhow!("workspace is not indexed"))?;
        let entry = manifest
            .files
            .get(path)
            .ok_or_else(|| anyhow::anyhow!("path is not in current snapshot"))?;
        if !token.authorize_path_hash(path, &entry.content_hash) {
            anyhow::bail!("path is not authorized by snapshot token");
        }
        let absolute = safe_workspace_join(root, path)?;
        let contents = fs::read_to_string(&absolute)?;
        Ok(AuthorizedFileRead {
            path: path.to_string(),
            content_hash: entry.content_hash.clone(),
            contents,
        })
    }

    pub fn start_workspace_watcher(&self, root: PathBuf, debounce: Duration) -> WorkspaceWatcher {
        self.start_workspace_watcher_inner(root, debounce, false)
            .expect("manual watcher does not create native watcher")
    }

    pub fn start_native_workspace_watcher(
        &self,
        root: PathBuf,
        debounce: Duration,
    ) -> Result<WorkspaceWatcher> {
        self.start_workspace_watcher_inner(root, debounce, true)
    }

    fn start_workspace_watcher_inner(
        &self,
        root: PathBuf,
        debounce: Duration,
        native: bool,
    ) -> Result<WorkspaceWatcher> {
        let engine = self.clone();
        let (event_tx, event_rx) = mpsc::channel::<PathBuf>();
        let native_event_tx = event_tx.clone();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let worker_root = root.clone();
        let handle = thread::spawn(move || {
            let mut pending = PendingFileEvents::default();
            loop {
                match event_rx.recv_timeout(debounce) {
                    Ok(path) => pending.push(path),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let _ = pending.flush(&engine, &worker_root);
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if stop_rx.try_recv().is_ok() {
                    let _ = pending.flush(&engine, &worker_root);
                    break;
                }
            }
        });
        let native_watcher = if native {
            let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    for path in event.paths {
                        let _ = native_event_tx.send(path);
                    }
                }
            })?;
            watcher.watch(&root, RecursiveMode::Recursive)?;
            Some(watcher)
        } else {
            None
        };
        Ok(WorkspaceWatcher {
            event_tx,
            stop_tx,
            handle: Some(handle),
            _native_watcher: native_watcher,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ManifestStore {
    base_dir: PathBuf,
}

impl ManifestStore {
    pub fn default_store() -> Result<Self> {
        Ok(Self {
            base_dir: jcode_storage::jcode_dir()?.join("codebase"),
        })
    }

    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn load(&self, root: &Path) -> Result<Option<Manifest>> {
        let git_state = read_git_state(root);
        let path = self.manifest_path_for_state(root, &git_state);
        if !path.exists() {
            return Ok(None);
        }
        jcode_storage::read_json(&path).map(Some)
    }

    pub fn save(&self, root: &Path, manifest: &Manifest) -> Result<()> {
        jcode_storage::write_json(&self.manifest_path_for_manifest(root, manifest), manifest)
    }

    pub fn load_status(&self, root: &Path) -> Result<Option<WorkspaceStatus>> {
        let path = self.status_path(root);
        if !path.exists() {
            return Ok(None);
        }
        jcode_storage::read_json(&path).map(Some)
    }

    pub fn save_status(&self, root: &Path, status: &WorkspaceStatus) -> Result<()> {
        jcode_storage::write_json_fast(&self.status_path(root), status)
    }

    pub fn manifest_path(&self, root: &Path) -> PathBuf {
        self.manifest_path_for_state(root, &read_git_state(root))
    }

    fn manifest_path_for_manifest(&self, root: &Path, manifest: &Manifest) -> PathBuf {
        self.base_dir
            .join(workspace_id(root))
            .join(branch_key(manifest.branch.as_deref(), manifest.head_sha.as_deref()))
            .join("manifest.json")
    }

    fn manifest_path_for_state(&self, root: &Path, git_state: &GitState) -> PathBuf {
        self.base_dir
            .join(workspace_id(root))
            .join(branch_key(git_state.branch.as_deref(), git_state.head_sha.as_deref()))
            .join("manifest.json")
    }

    fn status_path(&self, root: &Path) -> PathBuf {
        self.manifest_path(root).with_file_name("status.json")
    }
}

pub fn discover_filter_hash(root: &Path, rules: &IgnoreRules) -> Result<ScanResult> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut files = BTreeMap::new();
    let mut ignored_count = 0;
    let mut warnings = Vec::new();
    let walker = WalkBuilder::new(&root)
        .hidden(false)
        .follow_links(false)
        .build();

    for item in walker {
        let item = match item {
            Ok(item) => item,
            Err(err) => {
                warnings.push(err.to_string());
                continue;
            }
        };
        let path = item.path();
        if path == root {
            continue;
        }
        let Ok(relative_path) = path.strip_prefix(&root) else {
            ignored_count += 1;
            continue;
        };
        if rules.is_ignored(relative_path, item.file_type().map(|t| t.is_dir()).unwrap_or(false)) {
            ignored_count += 1;
            continue;
        }
        if !item.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if should_skip_path(relative_path) {
            ignored_count += 1;
            continue;
        }
        match scan_one_file(path, relative_path) {
            Ok(Some(entry)) => {
                files.insert(entry.path.clone(), entry);
            }
            Ok(None) => {
                ignored_count += 1;
            }
            Err(err) => {
                warnings.push(format!("{}: {}", relative_path.display(), err));
            }
        }
    }

    Ok(ScanResult {
        files,
        ignored_count,
        warnings,
    })
}

pub fn build_manifest(
    root: &Path,
    rules: &IgnoreRules,
    files: BTreeMap<String, FileEntry>,
) -> Result<Manifest> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let git_state = read_git_state(&root);
    Ok(Manifest {
        schema_version: SCHEMA_VERSION,
        workspace_id: workspace_id(&root),
        manifest_root: root.to_string_lossy().into_owned(),
        branch: git_state.branch,
        head_sha: git_state.head_sha,
        worktree_root: git_state.worktree_root,
        uncommitted_patch_hash: git_state.uncommitted_patch_hash,
        ignore_rules_hash: rules.hash().to_string(),
        files,
        created_at: Utc::now(),
    })
}

pub fn hash_changed_paths(
    root: &Path,
    paths: &[PathBuf],
    rules: &IgnoreRules,
) -> Result<BTreeMap<String, Option<FileEntry>>> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut changed = BTreeMap::new();
    for path in paths {
        let absolute = if path.is_absolute() { path.clone() } else { root.join(path) };
        let absolute = absolute.canonicalize().unwrap_or(absolute);
        let Ok(relative) = absolute.strip_prefix(&root) else {
            continue;
        };
        let normalized = normalize_relative_path(relative);
        if !absolute.exists() {
            changed.insert(normalized, None);
            continue;
        }
        if rules.is_ignored(relative, absolute.is_dir()) || should_skip_path(relative) || !absolute.is_file() {
            changed.insert(normalized, None);
            continue;
        }
        changed.insert(normalized, scan_one_file(&absolute, relative)?);
    }
    Ok(changed)
}

pub fn patch_manifest(
    previous: &Manifest,
    partial: BTreeMap<String, Option<FileEntry>>,
    root: &Path,
    rules: &IgnoreRules,
) -> Result<Manifest> {
    let mut files = previous.files.clone();
    for (path, entry) in partial {
        match entry {
            Some(entry) => {
                files.insert(path, entry);
            }
            None => {
                files.remove(&path);
            }
        }
    }
    build_manifest(root, rules, files)
}

pub fn diff_manifest(prev: Option<&Manifest>, next: &Manifest, reason: DeltaReason) -> Delta {
    let Some(prev) = prev else {
        return Delta {
            added: next.files.values().cloned().collect(),
            modified: Vec::new(),
            removed: Vec::new(),
            renamed: Vec::new(),
            reason,
        };
    };

    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut removed = Vec::new();

    for (path, next_file) in &next.files {
        match prev.files.get(path) {
            None => added.push(next_file.clone()),
            Some(prev_file) if prev_file.content_hash != next_file.content_hash => {
                modified.push(next_file.clone());
            }
            _ => {}
        }
    }

    for path in prev.files.keys() {
        if !next.files.contains_key(path) {
            removed.push(path.clone());
        }
    }

    let renamed = detect_renames(prev, next, &removed, &added);

    Delta {
        added,
        modified,
        removed,
        renamed,
        reason,
    }
}

fn detect_renames(
    prev: &Manifest,
    next: &Manifest,
    removed: &[String],
    added: &[FileEntry],
) -> Vec<RenameEntry> {
    let removed_by_hash: HashMap<_, _> = removed
        .iter()
        .filter_map(|path| {
            prev.files
                .get(path)
                .map(|entry| (entry.content_hash.as_str(), path.as_str()))
        })
        .collect();
    added
        .iter()
        .filter_map(|entry| {
            let from = removed_by_hash.get(entry.content_hash.as_str())?;
            let _ = next.files.get(&entry.path)?;
            Some(RenameEntry {
                from: (*from).to_string(),
                to: entry.path.clone(),
                entry: entry.clone(),
            })
        })
        .collect()
}

fn scan_one_file(path: &Path, relative_path: &Path) -> Result<Option<FileEntry>> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_TEXT_FILE_BYTES {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    if is_binary(&bytes) {
        return Ok(None);
    }
    let normalized = normalize_relative_path(relative_path);
    Ok(Some(FileEntry {
        path: normalized,
        content_hash: format!("sha256:{}", sha256_hex(&bytes)),
        size_bytes: metadata.len(),
        language: language_for_path(relative_path),
        is_generated: is_generated_path(relative_path),
    }))
}

fn chunk_text(entry: &FileEntry, text: &str) -> Vec<OverlayChunk> {
    let symbol_chunks = match entry.language.as_deref() {
        Some("rust") => chunk_symbols(entry, text, is_rust_symbol_start),
        Some("typescript") | Some("typescriptreact") | Some("javascript") | Some("javascriptreact") => {
            chunk_symbols(entry, text, is_js_symbol_start)
        }
        Some("python") => chunk_symbols(entry, text, is_python_symbol_start),
        _ => Vec::new(),
    };
    if !symbol_chunks.is_empty() {
        return symbol_chunks;
    }
    chunk_fixed_lines(entry, text)
}

fn chunk_fixed_lines(entry: &FileEntry, text: &str) -> Vec<OverlayChunk> {
    let lines: Vec<_> = text.lines().collect();
    if lines.is_empty() {
        return vec![OverlayChunk {
            path: entry.path.clone(),
            content_hash: entry.content_hash.clone(),
            start_line: 1,
            end_line: 1,
            text: String::new(),
        }];
    }
    lines
        .chunks(40)
        .enumerate()
        .map(|(index, chunk)| OverlayChunk {
            path: entry.path.clone(),
            content_hash: entry.content_hash.clone(),
            start_line: index * 40 + 1,
            end_line: index * 40 + chunk.len(),
            text: chunk.join("\n"),
        })
        .collect()
}

fn chunk_symbols(entry: &FileEntry, text: &str, is_symbol_start: fn(&str) -> bool) -> Vec<OverlayChunk> {
    let lines: Vec<_> = text.lines().collect();
    let starts: Vec<_> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| is_symbol_start(line.trim_start()).then_some(index))
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts
                .get(index + 1)
                .copied()
                .unwrap_or(lines.len())
                .min(start + 80);
            OverlayChunk {
                path: entry.path.clone(),
                content_hash: entry.content_hash.clone(),
                start_line: start + 1,
                end_line: end,
                text: lines[*start..end].join("\n"),
            }
        })
        .collect()
}

fn is_rust_symbol_start(line: &str) -> bool {
    let line = line.strip_prefix("pub ").unwrap_or(line);
    line.starts_with("fn ")
        || line.starts_with("async fn ")
        || line.starts_with("struct ")
        || line.starts_with("enum ")
        || line.starts_with("trait ")
        || line.starts_with("impl ")
}

fn is_js_symbol_start(line: &str) -> bool {
    let line = line
        .strip_prefix("export ")
        .or_else(|| line.strip_prefix("default "))
        .unwrap_or(line);
    line.starts_with("function ")
        || line.starts_with("async function ")
        || line.starts_with("class ")
        || line.starts_with("interface ")
        || line.starts_with("type ")
        || line.starts_with("const ") && line.contains("=>")
}

fn is_python_symbol_start(line: &str) -> bool {
    line.starts_with("def ") || line.starts_with("async def ") || line.starts_with("class ")
}

fn extract_symbols(entry: &FileEntry, text: &str) -> Vec<SymbolDefinition> {
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| extract_symbol_line(entry, line.trim_start(), index + 1))
        .collect()
}

fn extract_symbol_line(entry: &FileEntry, line: &str, start_line: usize) -> Option<SymbolDefinition> {
    let (kind, rest) = match entry.language.as_deref()? {
        "rust" => extract_rust_symbol(line)?,
        "typescript" | "typescriptreact" | "javascript" | "javascriptreact" => extract_js_symbol(line)?,
        "python" => extract_python_symbol(line)?,
        _ => return None,
    };
    Some(SymbolDefinition {
        path: entry.path.clone(),
        name: symbol_name(rest)?,
        kind: kind.to_string(),
        start_line,
    })
}

fn extract_rust_symbol(line: &str) -> Option<(&'static str, &str)> {
    let line = line.strip_prefix("pub ").unwrap_or(line);
    for (prefix, kind) in [("fn ", "function"), ("async fn ", "function"), ("struct ", "struct"), ("enum ", "enum"), ("trait ", "trait"), ("impl ", "impl")] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((kind, rest));
        }
    }
    None
}

fn extract_js_symbol(line: &str) -> Option<(&'static str, &str)> {
    let line = line.strip_prefix("export ").unwrap_or(line);
    for (prefix, kind) in [("function ", "function"), ("async function ", "function"), ("class ", "class"), ("interface ", "interface"), ("type ", "type"), ("const ", "function")] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((kind, rest));
        }
    }
    None
}

fn extract_python_symbol(line: &str) -> Option<(&'static str, &str)> {
    for (prefix, kind) in [("def ", "function"), ("async def ", "function"), ("class ", "class")] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((kind, rest));
        }
    }
    None
}

fn symbol_name(rest: &str) -> Option<String> {
    let name = rest
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .next()?
        .trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn test_source_path(path: &str) -> Option<String> {
    if let Some(source) = path.strip_suffix("_test.rs") {
        return Some(format!("{}.rs", source));
    }
    if let Some(source) = path.strip_suffix(".test.ts") {
        return Some(format!("{}.ts", source));
    }
    if let Some(source) = path.strip_suffix("_test.py") {
        return Some(format!("{}.py", source));
    }
    None
}

fn extract_import_targets(path: &str, text: &str, paths: &HashSet<String>) -> Vec<String> {
    let source_ext = Path::new(path).extension().and_then(|value| value.to_str());
    let base = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
    text.lines()
        .filter_map(|line| extract_import_path(line.trim()))
        .filter_map(|import| resolve_relative_import(base, import, source_ext, paths))
        .collect()
}

fn extract_import_path(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("mod ") {
        return Some(rest.trim_end_matches(';'));
    }
    if let Some(start) = line.find("from '") {
        return line[start + 6..].split('\'').next();
    }
    if let Some(start) = line.find("from \"") {
        return line[start + 6..].split('"').next();
    }
    None
}

fn resolve_relative_import(
    base: &Path,
    import: &str,
    source_ext: Option<&str>,
    paths: &HashSet<String>,
) -> Option<String> {
    let raw = if import.starts_with("./") {
        normalize_import_path(&base.join(import.trim_start_matches("./")))
    } else if import.starts_with('.') {
        normalize_import_path(&base.join(import))
    } else {
        normalize_import_path(&base.join(import.replace("::", "/")))
    };
    let mut candidates = vec![raw.clone()];
    if let Some(ext) = source_ext {
        candidates.push(format!("{}.{}", raw, ext));
    }
    candidates.extend([
        format!("{}.rs", raw),
        format!("{}.ts", raw),
        format!("{}.tsx", raw),
        format!("{}.py", raw),
        format!("{}/mod.rs", raw),
    ]);
    candidates
        .into_iter()
        .find(|candidate| paths.contains(candidate))
        .or_else(|| {
            let stem = Path::new(import).file_stem()?.to_str()?;
            paths
                .iter()
                .find(|candidate| {
                    Path::new(candidate).parent() == Some(base)
                        && Path::new(candidate).file_stem().and_then(|value| value.to_str()) == Some(stem)
                        && Path::new(candidate).extension().and_then(|value| value.to_str()) == source_ext
                })
                .cloned()
        })
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_alphanumeric() && ch != '-')
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

fn pop_ready(queue: &mut VecDeque<SyncQueueJob>) -> Option<SyncQueueJob> {
    let index = queue.iter().position(|job| job.retry_after_ms == 0)?;
    queue.remove(index)
}

fn retry_delay_ms(attempts: u32) -> u64 {
    1_000u64.saturating_mul(2u64.saturating_pow(attempts.min(6)))
}

fn embed_text(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0; 64];
    for term in query_terms(text) {
        let index = term.bytes().fold(0usize, |acc, byte| acc.wrapping_mul(31).wrapping_add(byte as usize)) % vector.len();
        vector[index] += 1.0;
    }
    vector
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    let mut dot = 0.0;
    let mut left_norm = 0.0;
    let mut right_norm = 0.0;
    for (left, right) in left.iter().zip(right.iter()) {
        dot += left * right;
        left_norm += left * left;
        right_norm += right * right;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        return 0.0;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
}

fn search_chunks(chunks_by_path: &BTreeMap<String, Vec<OverlayChunk>>, query: &str, limit: usize) -> Vec<OverlayHit> {
    let terms = query_terms(query);
    let mut hits = Vec::new();
    for chunks in chunks_by_path.values() {
        for chunk in chunks {
            let score = score_text(&chunk.path, &terms) * 3 + score_text(&chunk.text, &terms);
            if score > 0 {
                hits.push(OverlayHit {
                    chunk: chunk.clone(),
                    score,
                });
            }
        }
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.chunk.path.cmp(&b.chunk.path)));
    hits.truncate(limit);
    hits
}

fn status_from_manifest(manifest: &Manifest, warnings: Vec<String>) -> WorkspaceStatus {
    WorkspaceStatus {
        workspace_id: manifest.workspace_id.clone(),
        branch: manifest.branch.clone(),
        head_sha: manifest.head_sha.clone(),
        worktree_root: manifest.worktree_root.clone(),
        files_total: manifest.files.len(),
        last_manifest_at: manifest.created_at,
        file_statuses: manifest
            .files
            .values()
            .map(|entry| FileSyncStatus {
                path: entry.path.clone(),
                status: FileSyncState::IndexedLocalOnly,
                content_hash: Some(entry.content_hash.clone()),
                last_indexed_at: Some(manifest.created_at),
                error: None,
            })
            .collect(),
        warnings,
    }
}

pub fn workspace_id(root: &Path) -> String {
    let resolved = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    sha256_hex(resolved.to_string_lossy().as_bytes())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn normalize_relative_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn normalize_import_path(path: &Path) -> String {
    path.components()
        .filter(|component| !matches!(component, std::path::Component::CurDir))
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn should_skip_path(path: &Path) -> bool {
    let normalized = normalize_relative_path(path);
    let name = path.file_name().and_then(|v| v.to_str()).unwrap_or_default();
    let default_dirs: HashSet<&str> = [
        ".git",
        ".jcode",
        "node_modules",
        "dist",
        "build",
        "out",
        "target",
        "coverage",
        ".venv",
        "venv",
        "__pycache__",
    ]
    .into_iter()
    .collect();
    if path.components().any(|part| {
        default_dirs.contains(part.as_os_str().to_string_lossy().as_ref())
    }) {
        return true;
    }
    matches!(
        name,
        ".env" | "Cargo.lock" | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml"
    ) || normalized.ends_with(".min.js")
        || normalized.ends_with(".map")
        || normalized.ends_with(".pem")
        || normalized.ends_with(".key")
        || normalized.ends_with(".png")
        || normalized.ends_with(".jpg")
        || normalized.ends_with(".jpeg")
        || normalized.ends_with(".gif")
        || normalized.ends_with(".webp")
        || normalized.ends_with(".pdf")
        || normalized.ends_with(".zip")
        || normalized.ends_with(".tar")
        || normalized.ends_with(".gz")
        || normalized.starts_with(".env.")
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(4096).any(|byte| *byte == 0)
}

fn is_generated_path(path: &Path) -> bool {
    let normalized = normalize_relative_path(path);
    normalized.contains("/generated/") || normalized.ends_with(".min.js")
}

fn language_for_path(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?;
    let lang = match ext {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "json" => "json",
        "md" => "markdown",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        _ => return None,
    };
    Some(lang.to_string())
}

pub fn read_git_state(root: &Path) -> GitState {
    let branch = run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .filter(|value| value != "HEAD");
    let head_sha = run_git(root, &["rev-parse", "HEAD"]);
    let worktree_root = run_git(root, &["rev-parse", "--show-toplevel"]);
    let uncommitted_patch_hash = git_patch_hash(root);
    GitState {
        branch,
        head_sha,
        worktree_root,
        uncommitted_patch_hash,
    }
}

fn git_patch_hash(root: &Path) -> Option<String> {
    let mut data = Vec::new();
    let status = std::process::Command::new("git")
        .args(["diff", "--binary"])
        .current_dir(root)
        .output()
        .ok()?;
    if status.status.success() {
        data.extend(status.stdout);
    }
    let staged = std::process::Command::new("git")
        .args(["diff", "--cached", "--binary"])
        .current_dir(root)
        .output()
        .ok()?;
    if staged.status.success() {
        data.extend(staged.stdout);
    }
    (!data.is_empty()).then(|| format!("sha256:{}", sha256_hex(&data)))
}

fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn sanitize_hash_for_path(hash: &str) -> String {
    hash.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

fn safe_workspace_join(root: &Path, relative: &str) -> Result<PathBuf> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let path = Path::new(relative);
    if path.is_absolute() || path.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        anyhow::bail!("unsafe workspace path");
    }
    let joined = root.join(path);
    let canonical = joined.canonicalize()?;
    if !canonical.starts_with(&root) {
        anyhow::bail!("path escapes workspace");
    }
    Ok(canonical)
}

fn branch_key(branch: Option<&str>, head_sha: Option<&str>) -> String {
    let raw = branch.or(head_sha).unwrap_or("no-git");
    raw.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') { ch } else { '_' })
        .collect()
}

fn is_ignore_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| matches!(name, ".gitignore" | ".jcodeignore"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
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
    fn hash_is_content_stable() {
        assert_eq!(sha256_hex(b"same"), sha256_hex(b"same"));
        assert_ne!(sha256_hex(b"same"), sha256_hex(b"other"));
    }

    #[test]
    fn jcodeignore_excludes_files() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("keep.rs"), "fn keep() {}\n");
        write(&dir.path().join("skip.rs"), "fn skip() {}\n");
        write(&dir.path().join(".jcodeignore"), "skip.rs\n");
        let rules = IgnoreRules::load(dir.path()).unwrap();
        let scan = discover_filter_hash(dir.path(), &rules).unwrap();
        assert!(scan.files.contains_key("keep.rs"));
        assert!(!scan.files.contains_key("skip.rs"));
    }

    #[test]
    fn binary_and_secret_files_are_skipped() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("src/lib.rs"), "fn main() {}\n");
        fs::write(dir.path().join("image.bin"), b"a\0b").unwrap();
        write(&dir.path().join(".env"), "TOKEN=x\n");
        let rules = IgnoreRules::load(dir.path()).unwrap();
        let scan = discover_filter_hash(dir.path(), &rules).unwrap();
        assert!(scan.files.contains_key("src/lib.rs"));
        assert!(!scan.files.contains_key("image.bin"));
        assert!(!scan.files.contains_key(".env"));
    }

    #[test]
    fn diff_tracks_add_modify_delete_and_rename() {
        let mut prev_files = BTreeMap::new();
        prev_files.insert(
            "old.rs".to_string(),
            FileEntry {
                path: "old.rs".to_string(),
                content_hash: "sha256:a".to_string(),
                size_bytes: 1,
                language: None,
                is_generated: false,
            },
        );
        prev_files.insert(
            "change.rs".to_string(),
            FileEntry {
                path: "change.rs".to_string(),
                content_hash: "sha256:b".to_string(),
                size_bytes: 1,
                language: None,
                is_generated: false,
            },
        );
        let prev = Manifest {
            schema_version: 1,
            workspace_id: "w".to_string(),
            manifest_root: "/tmp/w".to_string(),
            branch: None,
            head_sha: None,
            worktree_root: None,
            uncommitted_patch_hash: None,
            ignore_rules_hash: "h".to_string(),
            files: prev_files,
            created_at: Utc::now(),
        };
        let mut next = prev.clone();
        next.files.remove("old.rs");
        next.files.insert(
            "new.rs".to_string(),
            FileEntry {
                path: "new.rs".to_string(),
                content_hash: "sha256:a".to_string(),
                size_bytes: 1,
                language: None,
                is_generated: false,
            },
        );
        next.files.insert(
            "change.rs".to_string(),
            FileEntry {
                path: "change.rs".to_string(),
                content_hash: "sha256:c".to_string(),
                size_bytes: 1,
                language: None,
                is_generated: false,
            },
        );
        let delta = diff_manifest(Some(&prev), &next, DeltaReason::FileWatch);
        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.modified.len(), 1);
        assert_eq!(delta.removed, vec!["old.rs"]);
        assert_eq!(delta.renamed.len(), 1);
    }

    #[test]
    fn commit_lineage_harvests_changed_files_and_terms() {
        let dir = TempDir::new().unwrap();
        run_git_cmd(dir.path(), &["init"]);
        run_git_cmd(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git_cmd(dir.path(), &["config", "user.name", "Test"]);
        write(&dir.path().join("src/search.rs"), "fn search_index() {}\n");
        run_git_cmd(dir.path(), &["add", "."]);
        run_git_cmd(dir.path(), &["commit", "-m", "add search index"]);
        let docs = harvest_commit_lineage(dir.path(), 5).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].changed_files, vec!["src/search.rs"]);
        assert!(docs[0].technical_terms.contains(&"search".to_string()));
    }

    #[test]
    fn dependency_graph_links_tests_to_sources() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn login() {}\n");
        write(&dir.path().join("src/app.ts"), "import { login } from './auth';\n");
        write(&dir.path().join("src/auth.ts"), "export function login() {}\n");
        write(&dir.path().join("src/auth_test.rs"), "#[test]\nfn login_test() {}\n");
        let rules = IgnoreRules::load(dir.path()).unwrap();
        let scan = discover_filter_hash(dir.path(), &rules).unwrap();
        let manifest = build_manifest(dir.path(), &rules, scan.files).unwrap();
        let graph = DependencyGraph::rebuild(dir.path(), &manifest).unwrap();
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "src/auth_test.rs" && edge.to == "src/auth.rs" && edge.kind == "test_source"
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "src/app.ts" && edge.to == "src/auth.ts" && edge.kind == "import"
        }));
    }

    #[test]
    fn symbol_index_extracts_rust_typescript_and_python_symbols() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("src/lib.rs"), "pub fn rust_login() {}\n");
        write(&dir.path().join("src/app.ts"), "export function tsLogin() {}\n");
        write(&dir.path().join("src/app.py"), "def py_login():\n    pass\n");
        let rules = IgnoreRules::load(dir.path()).unwrap();
        let scan = discover_filter_hash(dir.path(), &rules).unwrap();
        let manifest = build_manifest(dir.path(), &rules, scan.files).unwrap();
        let index = SymbolIndex::rebuild(dir.path(), &manifest).unwrap();
        let names: HashSet<_> = index.symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        assert!(names.contains("rust_login"));
        assert!(names.contains("tsLogin"));
        assert!(names.contains("py_login"));
    }

    #[test]
    fn exact_vector_index_returns_semantic_chunk() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn validate_password() {}\n");
        write(&dir.path().join("src/billing.rs"), "pub fn charge_card() {}\n");
        let rules = IgnoreRules::load(dir.path()).unwrap();
        let scan = discover_filter_hash(dir.path(), &rules).unwrap();
        let manifest = build_manifest(dir.path(), &rules, scan.files).unwrap();
        let overlay = LocalOverlayIndex::rebuild(dir.path(), &manifest).unwrap();
        let index = ExactVectorIndex::rebuild(&overlay);
        let hits = index.search("password validation", 5);
        assert_eq!(hits[0].chunk.path, "src/auth.rs");
    }

    #[test]
    fn sync_queue_drains_hot_before_bulk_and_shadow() {
        let mut scheduler = SyncQueueScheduler::default();
        scheduler.push(SyncQueueJob {
            id: "bulk".to_string(),
            priority: SyncPriority::Bulk,
            content_hashes: vec!["sha256:bulk".to_string()],
            attempts: 0,
            retry_after_ms: 0,
        });
        scheduler.push(SyncQueueJob {
            id: "shadow".to_string(),
            priority: SyncPriority::Shadow,
            content_hashes: vec!["sha256:shadow".to_string()],
            attempts: 0,
            retry_after_ms: 0,
        });
        scheduler.push(SyncQueueJob {
            id: "hot".to_string(),
            priority: SyncPriority::Hot,
            content_hashes: vec!["sha256:hot".to_string()],
            attempts: 0,
            retry_after_ms: 0,
        });
        scheduler.push(SyncQueueJob {
            id: "warm".to_string(),
            priority: SyncPriority::Warm,
            content_hashes: vec!["sha256:warm".to_string()],
            attempts: 0,
            retry_after_ms: 0,
        });
        assert_eq!(scheduler.pending_len(), 4);
        assert_eq!(scheduler.pop_next().unwrap().id, "hot");
        assert_eq!(scheduler.pop_next().unwrap().id, "warm");
        assert_eq!(scheduler.pop_next().unwrap().id, "bulk");
        assert_eq!(scheduler.pop_next().unwrap().id, "shadow");
        assert!(scheduler.pop_next().is_none());
    }

    #[test]
    fn failed_sync_job_gets_retry_backoff() {
        let mut scheduler = SyncQueueScheduler::default();
        scheduler.requeue_failed(SyncQueueJob {
            id: "hot".to_string(),
            priority: SyncPriority::Hot,
            content_hashes: vec!["sha256:hot".to_string()],
            attempts: 0,
            retry_after_ms: 0,
        });
        assert_eq!(scheduler.pending_len(), 1);
        assert!(scheduler.pop_next().is_none());
    }

    #[test]
    fn sync_outbox_replays_jobs_after_restart() {
        let dir = TempDir::new().unwrap();
        let outbox = SyncOutbox::new(dir.path().join("outbox.jsonl"));
        outbox
            .append(&SyncQueueJob {
                id: "hot".to_string(),
                priority: SyncPriority::Hot,
                content_hashes: vec!["sha256:hot".to_string()],
                attempts: 0,
                retry_after_ms: 0,
            })
            .unwrap();
        outbox
            .append(&SyncQueueJob {
                id: "bulk".to_string(),
                priority: SyncPriority::Bulk,
                content_hashes: vec!["sha256:bulk".to_string()],
                attempts: 0,
                retry_after_ms: 0,
            })
            .unwrap();
        let mut scheduler = outbox.load().unwrap();
        assert_eq!(scheduler.pending_len(), 2);
        assert_eq!(scheduler.pop_next().unwrap().id, "hot");
        assert_eq!(scheduler.pop_next().unwrap().id, "bulk");
        outbox.clear().unwrap();
        assert_eq!(outbox.load().unwrap().pending_len(), 0);
    }

    #[test]
    fn sync_outbox_status_reports_priority_depths() {
        let dir = TempDir::new().unwrap();
        let outbox = SyncOutbox::new(dir.path().join("outbox.jsonl"));
        outbox
            .append(&SyncQueueJob {
                id: "hot".to_string(),
                priority: SyncPriority::Hot,
                content_hashes: vec!["sha256:hot".to_string()],
                attempts: 0,
                retry_after_ms: 0,
            })
            .unwrap();
        outbox
            .append(&SyncQueueJob {
                id: "bulk".to_string(),
                priority: SyncPriority::Bulk,
                content_hashes: vec!["sha256:bulk".to_string()],
                attempts: 0,
                retry_after_ms: 0,
            })
            .unwrap();
        let status = outbox.status_summary(3, 2).unwrap();
        assert_eq!(status.pending_uploads, 2);
        assert_eq!(status.blobs_total, 3);
        assert_eq!(status.committed_deltas, 2);
        assert_eq!(status.queue_hot, 1);
        assert_eq!(status.queue_warm, 0);
        assert_eq!(status.queue_bulk, 1);
        assert_eq!(status.queue_shadow, 0);
    }

    #[test]
    fn sync_outbox_drains_ready_job_into_client() {
        let dir = TempDir::new().unwrap();
        let cas_dir = TempDir::new().unwrap();
        write(&dir.path().join("src/lib.rs"), "fn drain() {}\n");
        let rules = IgnoreRules::load(dir.path()).unwrap();
        let scan = discover_filter_hash(dir.path(), &rules).unwrap();
        let hash = scan.files.values().next().unwrap().content_hash.clone();
        let outbox = SyncOutbox::new(dir.path().join("outbox.jsonl"));
        outbox
            .append(&SyncQueueJob {
                id: "hot".to_string(),
                priority: SyncPriority::Hot,
                content_hashes: vec![hash],
                attempts: 0,
                retry_after_ms: 0,
            })
            .unwrap();
        let client = LocalCasSyncClient::new(cas_dir.path().to_path_buf());
        assert_eq!(outbox.drain_ready(dir.path(), &client).unwrap(), 1);
        assert_eq!(outbox.load().unwrap().pending_len(), 0);
        assert_eq!(client.sync_status().unwrap().committed_deltas, 1);
    }

    #[test]
    fn local_cas_uploads_missing_blobs_once_and_commits_delta() {
        let dir = TempDir::new().unwrap();
        let cas_dir = TempDir::new().unwrap();
        write(&dir.path().join("src/lib.rs"), "fn cas() {}\n");
        let rules = IgnoreRules::load(dir.path()).unwrap();
        let scan = discover_filter_hash(dir.path(), &rules).unwrap();
        let entries: Vec<_> = scan.files.values().cloned().collect();
        let client = LocalCasSyncClient::new(cas_dir.path().to_path_buf());
        let hashes: Vec<_> = entries.iter().map(|entry| entry.content_hash.clone()).collect();
        assert_eq!(client.find_missing_blobs(&hashes).unwrap(), hashes);
        client.upload_blobs(&entries, dir.path()).unwrap();
        assert!(client.find_missing_blobs(&hashes).unwrap().is_empty());
        client.upload_blobs(&entries, dir.path()).unwrap();
        client
            .commit_delta(CommitDeltaRequest {
                added: entries,
                modified: Vec::new(),
                removed: Vec::new(),
                renamed: Vec::new(),
                priority: SyncPriority::Hot,
            })
            .unwrap();
        let status = client.sync_status().unwrap();
        assert_eq!(status.blobs_total, 1);
        assert_eq!(status.committed_deltas, 1);
    }

    #[test]
    fn snapshot_token_authorizes_current_path_hash_only() {
        let dir = TempDir::new().unwrap();
        let store_dir = TempDir::new().unwrap();
        write(&dir.path().join("src/lib.rs"), "fn allowed() {}\n");
        write(&dir.path().join("src/other.rs"), "fn other() {}\n");
        let engine = CodebaseSyncEngine::new(ManifestStore::new(store_dir.path().to_path_buf()));
        engine.open_workspace(dir.path()).unwrap();
        let token = engine.snapshot_token(dir.path()).unwrap().unwrap();
        let read = engine
            .read_file_authorized(dir.path(), "src/lib.rs", &token)
            .unwrap();
        assert!(read.contents.contains("allowed"));
        let mut tampered = token.clone();
        tampered
            .path_to_hash
            .insert("src/lib.rs".to_string(), "sha256:bad".to_string());
        assert!(engine.read_file_authorized(dir.path(), "src/lib.rs", &tampered).is_err());
        assert!(engine.read_file_authorized(dir.path(), "../src/lib.rs", &token).is_err());
    }

    #[test]
    fn native_workspace_watcher_rescans_on_ignore_rule_change() {
        let dir = TempDir::new().unwrap();
        let store_dir = TempDir::new().unwrap();
        write(&dir.path().join("src/lib.rs"), "fn kept() {}\n");
        let ignore = dir.path().join(".jcodeignore");
        let engine = CodebaseSyncEngine::new(ManifestStore::new(store_dir.path().to_path_buf()));
        engine.open_workspace(dir.path()).unwrap();
        let watcher = engine
            .start_native_workspace_watcher(dir.path().to_path_buf(), Duration::from_millis(20))
            .unwrap();
        write(&ignore, "src/lib.rs\n");
        thread::sleep(Duration::from_millis(250));
        watcher.stop().unwrap();
        let manifest = engine.store.load(dir.path()).unwrap().unwrap();
        assert!(!manifest.files.contains_key("src/lib.rs"));
    }

    #[test]
    fn native_workspace_watcher_flushes_filesystem_events() {
        let dir = TempDir::new().unwrap();
        let store_dir = TempDir::new().unwrap();
        let file = dir.path().join("src/lib.rs");
        write(&file, "fn before() {}\n");
        let engine = CodebaseSyncEngine::new(ManifestStore::new(store_dir.path().to_path_buf()));
        let (before, _) = engine.open_workspace(dir.path()).unwrap();
        let before_hash = before.files["src/lib.rs"].content_hash.clone();
        let watcher = engine
            .start_native_workspace_watcher(dir.path().to_path_buf(), Duration::from_millis(20))
            .unwrap();
        write(&file, "fn after_native() {}\n");
        thread::sleep(Duration::from_millis(200));
        watcher.stop().unwrap();
        let manifest = engine.store.load(dir.path()).unwrap().unwrap();
        assert_ne!(manifest.files["src/lib.rs"].content_hash, before_hash);
    }

    #[test]
    fn workspace_watcher_flushes_debounced_events() {
        let dir = TempDir::new().unwrap();
        let store_dir = TempDir::new().unwrap();
        let file = dir.path().join("src/lib.rs");
        write(&file, "fn before() {}\n");
        let engine = CodebaseSyncEngine::new(ManifestStore::new(store_dir.path().to_path_buf()));
        engine.open_workspace(dir.path()).unwrap();
        let watcher = engine.start_workspace_watcher(dir.path().to_path_buf(), Duration::from_millis(20));
        write(&file, "fn after() {}\n");
        watcher.push_path(file).unwrap();
        thread::sleep(Duration::from_millis(80));
        watcher.stop().unwrap();
        let manifest = engine.store.load(dir.path()).unwrap().unwrap();
        assert!(manifest.files["src/lib.rs"].content_hash.starts_with("sha256:"));
    }

    #[test]
    fn pending_file_events_dedupe_and_flush() {
        let dir = TempDir::new().unwrap();
        let store_dir = TempDir::new().unwrap();
        let file = dir.path().join("src/lib.rs");
        write(&file, "fn before() {}\n");
        let engine = CodebaseSyncEngine::new(ManifestStore::new(store_dir.path().to_path_buf()));
        engine.open_workspace(dir.path()).unwrap();
        write(&file, "fn after() {}\n");
        let mut pending = PendingFileEvents::default();
        pending.push(file.clone());
        pending.push(file);
        let (_, delta) = pending.flush(&engine, dir.path()).unwrap().unwrap();
        assert_eq!(delta.modified.len(), 1);
        assert!(pending.is_empty());
    }

    #[test]
    fn overlay_search_prefers_changed_file_content() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("src/auth.rs"), "pub fn login() {\n    validate_password();\n}\n");
        let rules = IgnoreRules::load(dir.path()).unwrap();
        let scan = discover_filter_hash(dir.path(), &rules).unwrap();
        let manifest = build_manifest(dir.path(), &rules, scan.files).unwrap();
        let overlay = LocalOverlayIndex::rebuild(dir.path(), &manifest).unwrap();
        let hits = overlay.search("validate_password", 5);
        assert_eq!(hits[0].chunk.path, "src/auth.rs");
    }

    #[test]
    fn typescript_and_python_overlay_chunks_follow_symbol_boundaries() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("src/app.ts"),
            "export function first() {\n  one();\n}\n\nexport function second() {\n  two();\n}\n",
        );
        write(
            &dir.path().join("src/app.py"),
            "def first():\n    one()\n\ndef second():\n    two()\n",
        );
        let rules = IgnoreRules::load(dir.path()).unwrap();
        let scan = discover_filter_hash(dir.path(), &rules).unwrap();
        let manifest = build_manifest(dir.path(), &rules, scan.files).unwrap();
        let overlay = LocalOverlayIndex::rebuild(dir.path(), &manifest).unwrap();
        let ts_hits = overlay.search("second two", 5);
        assert!(ts_hits.iter().any(|hit| hit.chunk.path == "src/app.ts" && !hit.chunk.text.contains("first")));
        let py_hits = overlay.search("second two", 5);
        assert!(py_hits.iter().any(|hit| hit.chunk.path == "src/app.py" && !hit.chunk.text.contains("first")));
    }

    #[test]
    fn rust_overlay_chunks_follow_symbol_boundaries() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("src/lib.rs"),
            "pub fn first() {\n    one();\n}\n\npub fn second() {\n    two();\n}\n",
        );
        let rules = IgnoreRules::load(dir.path()).unwrap();
        let scan = discover_filter_hash(dir.path(), &rules).unwrap();
        let manifest = build_manifest(dir.path(), &rules, scan.files).unwrap();
        let overlay = LocalOverlayIndex::rebuild(dir.path(), &manifest).unwrap();
        let hits = overlay.search("second two", 5);
        assert_eq!(hits[0].chunk.start_line, 5);
        assert!(hits[0].chunk.text.contains("pub fn second"));
        assert!(!hits[0].chunk.text.contains("pub fn first"));
    }

    #[test]
    fn file_change_hashes_only_changed_paths() {
        let dir = TempDir::new().unwrap();
        let store_dir = TempDir::new().unwrap();
        let first = dir.path().join("src/first.rs");
        let second = dir.path().join("src/second.rs");
        write(&first, "fn first() {}\n");
        write(&second, "fn second() {}\n");
        let engine = CodebaseSyncEngine::new(ManifestStore::new(store_dir.path().to_path_buf()));
        engine.open_workspace(dir.path()).unwrap();
        write(&first, "fn first_changed() {}\n");
        let (_, delta) = engine.on_file_change(dir.path(), &[first]).unwrap();
        assert_eq!(delta.modified.len(), 1);
        assert_eq!(delta.modified[0].path, "src/first.rs");
        assert!(delta.added.is_empty());
        assert!(delta.removed.is_empty());
    }

    #[test]
    fn ignore_file_change_triggers_full_rescan_reason() {
        let dir = TempDir::new().unwrap();
        let store_dir = TempDir::new().unwrap();
        write(&dir.path().join("src/lib.rs"), "fn lib() {}\n");
        let ignore = dir.path().join(".jcodeignore");
        let engine = CodebaseSyncEngine::new(ManifestStore::new(store_dir.path().to_path_buf()));
        engine.open_workspace(dir.path()).unwrap();
        write(&ignore, "src/lib.rs\n");
        let (_, delta) = engine.on_file_change(dir.path(), &[ignore]).unwrap();
        assert_eq!(delta.reason, DeltaReason::IgnoreRulesChanged);
        assert_eq!(delta.removed, vec!["src/lib.rs"]);
    }

    #[test]
    fn branch_manifests_do_not_overwrite_each_other() {
        let dir = TempDir::new().unwrap();
        run_git_cmd(dir.path(), &["init"]);
        run_git_cmd(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git_cmd(dir.path(), &["config", "user.name", "Test"]);
        write(&dir.path().join("src/lib.rs"), "pub fn branch_value() -> &'static str { \"main\" }\n");
        run_git_cmd(dir.path(), &["add", "."]);
        run_git_cmd(dir.path(), &["commit", "-m", "main"]);
        let store_dir = TempDir::new().unwrap();
        let engine = CodebaseSyncEngine::new(ManifestStore::new(store_dir.path().to_path_buf()));
        let (main_manifest, _) = engine.open_workspace(dir.path()).unwrap();
        run_git_cmd(dir.path(), &["checkout", "-b", "feature"]);
        write(&dir.path().join("src/lib.rs"), "pub fn branch_value() -> &'static str { \"feature\" }\n");
        run_git_cmd(dir.path(), &["add", "."]);
        run_git_cmd(dir.path(), &["commit", "-m", "feature"]);
        let (feature_manifest, _) = engine.open_workspace(dir.path()).unwrap();
        assert_ne!(main_manifest.head_sha, feature_manifest.head_sha);
        assert_ne!(
            main_manifest.files["src/lib.rs"].content_hash,
            feature_manifest.files["src/lib.rs"].content_hash
        );
        run_git_cmd(dir.path(), &["checkout", main_manifest.branch.as_deref().unwrap()]);
        let reloaded = engine.status(dir.path()).unwrap().unwrap();
        assert_eq!(reloaded.head_sha, main_manifest.head_sha);
    }

    #[test]
    fn engine_persists_manifest_and_skips_mtime_only_change() {
        let dir = TempDir::new().unwrap();
        let store_dir = TempDir::new().unwrap();
        let file = dir.path().join("src/lib.rs");
        write(&file, "fn a() {}\n");
        let engine = CodebaseSyncEngine::new(ManifestStore::new(store_dir.path().to_path_buf()));
        let (_, first_delta) = engine.open_workspace(dir.path()).unwrap();
        assert_eq!(first_delta.added.len(), 1);
        let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
        f.flush().unwrap();
        let (_, second_delta) = engine.on_file_change(dir.path(), &[file]).unwrap();
        assert!(second_delta.added.is_empty());
        assert!(second_delta.modified.is_empty());
        assert!(second_delta.removed.is_empty());
    }
}
