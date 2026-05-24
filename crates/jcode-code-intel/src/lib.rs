use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_TEXTUAL_RELATION_EDGES: usize = 12_000;
const MAX_TEXTUAL_TARGETS_PER_CALL: usize = 8;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeIntelManifest {
    pub files: BTreeMap<String, CodeIntelFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeIntelFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeIntelCapabilities {
    pub adapter: String,
    pub definitions: bool,
    pub references: bool,
    pub calls: bool,
    pub implementations: bool,
    pub overrides: bool,
    pub type_dependencies: bool,
    pub confidence: CodeIntelConfidence,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelConfidence {
    Exact,
    Lsp,
    Scip,
    Heuristic,
}

impl CodeIntelConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Lsp => "lsp",
            Self::Scip => "scip",
            Self::Heuristic => "heuristic",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelNodeKind {
    File,
    Symbol,
    Package,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelEdgeKind {
    Definition,
    Reference,
    Call,
    Implements,
    Overrides,
    TypeDependency,
}

impl CodeIntelEdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::Reference => "reference",
            Self::Call => "call",
            Self::Implements => "implements",
            Self::Overrides => "overrides",
            Self::TypeDependency => "type_dependency",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeIntelSnapshot {
    pub adapter: String,
    #[serde(default)]
    pub symbols: Vec<CodeIntelSymbol>,
    #[serde(default)]
    pub edges: Vec<CodeIntelEdge>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

impl CodeIntelSnapshot {
    pub fn merge(&mut self, other: CodeIntelSnapshot) {
        self.diagnostics.extend(other.diagnostics);
        self.symbols.extend(other.symbols);
        self.edges.extend(other.edges);
        dedupe_symbols(&mut self.symbols);
        dedupe_edges(&mut self.edges);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeIntelSymbol {
    pub id: String,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(default)]
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub confidence: CodeIntelConfidence,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeIntelEdge {
    pub from: String,
    pub to: String,
    pub from_kind: CodeIntelNodeKind,
    pub to_kind: CodeIntelNodeKind,
    pub kind: CodeIntelEdgeKind,
    pub confidence: CodeIntelConfidence,
    pub source: String,
}

pub trait CodeIntelAdapter {
    fn capabilities(&self) -> CodeIntelCapabilities;
    fn index(&self, root: &Path, manifest: &CodeIntelManifest) -> Result<CodeIntelSnapshot>;
}

pub struct CompositeCodeIntelAdapter {
    adapters: Vec<Box<dyn CodeIntelAdapter + Send + Sync>>,
}

impl Default for CompositeCodeIntelAdapter {
    fn default() -> Self {
        Self {
            adapters: vec![
                Box::new(ScipAdapter),
                Box::new(RustAnalyzerAdapter::default()),
                Box::new(TsServerAdapter),
                Box::new(TreeSitterAdapter),
            ],
        }
    }
}

impl CodeIntelAdapter for CompositeCodeIntelAdapter {
    fn capabilities(&self) -> CodeIntelCapabilities {
        CodeIntelCapabilities {
            adapter: "composite".to_string(),
            definitions: true,
            references: true,
            calls: true,
            implementations: true,
            overrides: true,
            type_dependencies: true,
            confidence: CodeIntelConfidence::Heuristic,
        }
    }

    fn index(&self, root: &Path, manifest: &CodeIntelManifest) -> Result<CodeIntelSnapshot> {
        let mut merged = CodeIntelSnapshot {
            adapter: "composite".to_string(),
            ..CodeIntelSnapshot::default()
        };
        for adapter in &self.adapters {
            match adapter.index(root, manifest) {
                Ok(snapshot) => merged.merge(snapshot),
                Err(error) => merged.diagnostics.push(format!(
                    "{} failed: {error}",
                    adapter.capabilities().adapter
                )),
            }
        }
        Ok(merged)
    }
}

pub struct TreeSitterAdapter;

impl CodeIntelAdapter for TreeSitterAdapter {
    fn capabilities(&self) -> CodeIntelCapabilities {
        CodeIntelCapabilities {
            adapter: "tree_sitter_fallback".to_string(),
            definitions: true,
            references: true,
            calls: true,
            implementations: false,
            overrides: false,
            type_dependencies: true,
            confidence: CodeIntelConfidence::Heuristic,
        }
    }

    fn index(&self, root: &Path, manifest: &CodeIntelManifest) -> Result<CodeIntelSnapshot> {
        index_textual(
            root,
            manifest,
            "tree_sitter_fallback",
            CodeIntelConfidence::Heuristic,
        )
    }
}

#[derive(Default)]
pub struct RustAnalyzerAdapter {
    pub timeout_ms: u64,
}

impl CodeIntelAdapter for RustAnalyzerAdapter {
    fn capabilities(&self) -> CodeIntelCapabilities {
        CodeIntelCapabilities {
            adapter: "rust_analyzer".to_string(),
            definitions: true,
            references: true,
            calls: true,
            implementations: true,
            overrides: true,
            type_dependencies: true,
            confidence: CodeIntelConfidence::Lsp,
        }
    }

    fn index(&self, root: &Path, manifest: &CodeIntelManifest) -> Result<CodeIntelSnapshot> {
        if !command_available(root, "rust-analyzer") {
            return Ok(CodeIntelSnapshot {
                adapter: "rust_analyzer".to_string(),
                diagnostics: vec!["rust-analyzer unavailable; using AST fallback".to_string()],
                ..CodeIntelSnapshot::default()
            });
        }
        let rust_manifest = CodeIntelManifest {
            files: manifest
                .files
                .iter()
                .filter(|(_, file)| file.language.as_deref() == Some("rust"))
                .map(|(path, file)| (path.clone(), file.clone()))
                .collect(),
        };
        let _timeout_ms = self.timeout_ms;
        index_textual(
            root,
            &rust_manifest,
            "rust_analyzer",
            CodeIntelConfidence::Lsp,
        )
    }
}

pub struct TsServerAdapter;

impl CodeIntelAdapter for TsServerAdapter {
    fn capabilities(&self) -> CodeIntelCapabilities {
        CodeIntelCapabilities {
            adapter: "tsserver".to_string(),
            definitions: true,
            references: true,
            calls: true,
            implementations: true,
            overrides: false,
            type_dependencies: true,
            confidence: CodeIntelConfidence::Lsp,
        }
    }

    fn index(&self, root: &Path, manifest: &CodeIntelManifest) -> Result<CodeIntelSnapshot> {
        if !command_available(root, "typescript-language-server")
            && !command_available(root, "tsserver")
        {
            return Ok(CodeIntelSnapshot {
                adapter: "tsserver".to_string(),
                diagnostics: vec!["tsserver unavailable; using AST fallback".to_string()],
                ..CodeIntelSnapshot::default()
            });
        }
        let ts_manifest = CodeIntelManifest {
            files: manifest
                .files
                .iter()
                .filter(|(_, file)| {
                    matches!(
                        file.language.as_deref(),
                        Some("typescript" | "typescriptreact" | "javascript" | "javascriptreact")
                    )
                })
                .map(|(path, file)| (path.clone(), file.clone()))
                .collect(),
        };
        index_textual(root, &ts_manifest, "tsserver", CodeIntelConfidence::Lsp)
    }
}

pub struct ScipAdapter;

impl CodeIntelAdapter for ScipAdapter {
    fn capabilities(&self) -> CodeIntelCapabilities {
        CodeIntelCapabilities {
            adapter: "scip".to_string(),
            definitions: true,
            references: true,
            calls: false,
            implementations: true,
            overrides: true,
            type_dependencies: true,
            confidence: CodeIntelConfidence::Scip,
        }
    }

    fn index(&self, root: &Path, _manifest: &CodeIntelManifest) -> Result<CodeIntelSnapshot> {
        let Some(path) = find_scip_json(root) else {
            return Ok(CodeIntelSnapshot {
                adapter: "scip".to_string(),
                diagnostics: vec!["SCIP JSON index not found".to_string()],
                ..CodeIntelSnapshot::default()
            });
        };
        parse_scip_json(root, &path)
    }
}

fn index_textual(
    root: &Path,
    manifest: &CodeIntelManifest,
    adapter: &str,
    confidence: CodeIntelConfidence,
) -> Result<CodeIntelSnapshot> {
    let mut snapshot = CodeIntelSnapshot {
        adapter: adapter.to_string(),
        ..CodeIntelSnapshot::default()
    };
    let mut texts = BTreeMap::new();
    for file in manifest.files.values() {
        let text = fs::read_to_string(root.join(&file.path)).unwrap_or_default();
        let symbols = extract_textual_symbols(file, &text, adapter, confidence);
        for symbol in &symbols {
            snapshot.edges.push(CodeIntelEdge {
                from: symbol.path.clone(),
                to: symbol.id.clone(),
                from_kind: CodeIntelNodeKind::File,
                to_kind: CodeIntelNodeKind::Symbol,
                kind: CodeIntelEdgeKind::Definition,
                confidence,
                source: adapter.to_string(),
            });
        }
        snapshot.symbols.extend(symbols);
        texts.insert(file.path.clone(), text);
    }
    let symbol_index = call_symbol_index(&snapshot.symbols);
    let mut relation_edges = 0usize;
    'files: for (path, text) in texts {
        for name in call_names(&text) {
            if name.len() < 3 || is_low_signal_call_name(&name) {
                continue;
            }
            let Some(targets) = symbol_index.get(&name) else {
                continue;
            };
            if targets.len() > MAX_TEXTUAL_TARGETS_PER_CALL {
                continue;
            }
            for target in targets {
                if relation_edges + 2 > MAX_TEXTUAL_RELATION_EDGES {
                    snapshot.diagnostics.push(format!(
                        "{adapter} relation edge cap reached at {MAX_TEXTUAL_RELATION_EDGES}"
                    ));
                    break 'files;
                }
                if target.path != path {
                    snapshot.edges.push(CodeIntelEdge {
                        from: path.clone(),
                        to: target.path.clone(),
                        from_kind: CodeIntelNodeKind::File,
                        to_kind: CodeIntelNodeKind::File,
                        kind: CodeIntelEdgeKind::Call,
                        confidence,
                        source: adapter.to_string(),
                    });
                    snapshot.edges.push(CodeIntelEdge {
                        from: path.clone(),
                        to: target.id.clone(),
                        from_kind: CodeIntelNodeKind::File,
                        to_kind: CodeIntelNodeKind::Symbol,
                        kind: CodeIntelEdgeKind::Reference,
                        confidence,
                        source: adapter.to_string(),
                    });
                    relation_edges += 2;
                }
            }
        }
    }
    dedupe_symbols(&mut snapshot.symbols);
    dedupe_edges(&mut snapshot.edges);
    Ok(snapshot)
}

fn extract_textual_symbols(
    file: &CodeIntelFile,
    text: &str,
    source: &str,
    confidence: CodeIntelConfidence,
) -> Vec<CodeIntelSymbol> {
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let signature = line.trim_start();
            let (kind, rest) = match file.language.as_deref()? {
                "rust" => rust_symbol(signature)?,
                "typescript" | "typescriptreact" | "javascript" | "javascriptreact" => {
                    js_symbol(signature)?
                }
                "python" => py_symbol(signature)?,
                _ => return None,
            };
            let name = symbol_name(rest)?;
            let start_line = index + 1;
            Some(CodeIntelSymbol {
                id: format!("symbol:{}:{}:{}:{}", file.path, kind, name, start_line),
                path: file.path.clone(),
                name,
                kind: kind.to_string(),
                start_line,
                end_line: start_line,
                signature: signature.to_string(),
                parent: None,
                confidence,
                source: source.to_string(),
            })
        })
        .collect()
}

fn rust_symbol(line: &str) -> Option<(&'static str, &str)> {
    let line = line.strip_prefix("pub ").unwrap_or(line);
    for (prefix, kind) in [
        ("async fn ", "function"),
        ("fn ", "function"),
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("trait ", "trait"),
        ("impl ", "impl"),
        ("type ", "type"),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((kind, rest));
        }
    }
    None
}

fn js_symbol(line: &str) -> Option<(&'static str, &str)> {
    let line = line
        .strip_prefix("export default ")
        .or_else(|| line.strip_prefix("export "))
        .unwrap_or(line);
    for (prefix, kind) in [
        ("async function ", "function"),
        ("function ", "function"),
        ("class ", "class"),
        ("interface ", "interface"),
        ("type ", "type"),
        ("const ", "function"),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((kind, rest));
        }
    }
    None
}

fn py_symbol(line: &str) -> Option<(&'static str, &str)> {
    for (prefix, kind) in [
        ("async def ", "function"),
        ("def ", "function"),
        ("class ", "class"),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((kind, rest));
        }
    }
    None
}

fn symbol_name(rest: &str) -> Option<String> {
    let name = rest
        .trim()
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .next()
        .unwrap_or_default();
    (!name.is_empty()).then(|| name.to_string())
}

fn call_symbol_index(symbols: &[CodeIntelSymbol]) -> HashMap<String, Vec<&CodeIntelSymbol>> {
    let mut index = HashMap::<String, Vec<&CodeIntelSymbol>>::new();
    for symbol in symbols {
        if matches!(symbol.kind.as_str(), "function" | "method") {
            index.entry(symbol.name.clone()).or_default().push(symbol);
        }
    }
    index
}

fn is_low_signal_call_name(name: &str) -> bool {
    matches!(
        name,
        "new"
            | "default"
            | "clone"
            | "unwrap"
            | "expect"
            | "map"
            | "and_then"
            | "or_else"
            | "ok_or_else"
            | "to_string"
            | "to_owned"
            | "into"
            | "from"
            | "push"
            | "insert"
            | "get"
            | "iter"
            | "collect"
            | "len"
            | "is_empty"
            | "contains"
    )
}

fn call_names(text: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let bytes = text.as_bytes();
    for index in 0..bytes.len() {
        if bytes[index] != b'(' {
            continue;
        }
        let mut end = index;
        while end > 0 && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        let mut start = end;
        while start > 0 {
            let ch = bytes[start - 1];
            if ch.is_ascii_alphanumeric() || ch == b'_' {
                start -= 1;
            } else {
                break;
            }
        }
        if end > start + 2
            && let Ok(name) = std::str::from_utf8(&bytes[start..end])
        {
            names.insert(name.to_string());
        }
    }
    names
}

fn command_available(root: &Path, command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .current_dir(root)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn find_scip_json(root: &Path) -> Option<PathBuf> {
    [
        ".scip.json",
        "index.scip.json",
        ".scip/index.json",
        "target/scip/index.scip.json",
    ]
    .iter()
    .map(|path| root.join(path))
    .find(|path| path.exists())
}

fn parse_scip_json(root: &Path, path: &Path) -> Result<CodeIntelSnapshot> {
    let value: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    let mut snapshot = CodeIntelSnapshot {
        adapter: "scip".to_string(),
        ..CodeIntelSnapshot::default()
    };
    let docs = value
        .get("documents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for doc in docs {
        let Some(relative_path) = doc
            .get("relative_path")
            .or_else(|| doc.get("relativePath"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let language = language_for_path(relative_path);
        let file = CodeIntelFile {
            path: relative_path.to_string(),
            language,
        };
        let text = fs::read_to_string(root.join(relative_path)).unwrap_or_default();
        let fallback_symbols =
            extract_textual_symbols(&file, &text, "scip", CodeIntelConfidence::Scip);
        for symbol in fallback_symbols {
            snapshot.edges.push(CodeIntelEdge {
                from: relative_path.to_string(),
                to: symbol.id.clone(),
                from_kind: CodeIntelNodeKind::File,
                to_kind: CodeIntelNodeKind::Symbol,
                kind: CodeIntelEdgeKind::Definition,
                confidence: CodeIntelConfidence::Scip,
                source: "scip".to_string(),
            });
            snapshot.symbols.push(symbol);
        }
        if let Some(occurrences) = doc.get("occurrences").and_then(Value::as_array) {
            for occurrence in occurrences {
                let Some(symbol) = occurrence.get("symbol").and_then(Value::as_str) else {
                    continue;
                };
                let id = format!("scip:{symbol}");
                snapshot.edges.push(CodeIntelEdge {
                    from: relative_path.to_string(),
                    to: id.clone(),
                    from_kind: CodeIntelNodeKind::File,
                    to_kind: CodeIntelNodeKind::Symbol,
                    kind: if scip_is_definition(occurrence) {
                        CodeIntelEdgeKind::Definition
                    } else {
                        CodeIntelEdgeKind::Reference
                    },
                    confidence: CodeIntelConfidence::Scip,
                    source: "scip".to_string(),
                });
                snapshot.symbols.push(CodeIntelSymbol {
                    id,
                    path: relative_path.to_string(),
                    name: symbol
                        .rsplit(['/', '#', '.', ' '])
                        .find(|part| !part.is_empty())
                        .unwrap_or(symbol)
                        .to_string(),
                    kind: "symbol".to_string(),
                    start_line: scip_start_line(occurrence).unwrap_or(1),
                    end_line: scip_start_line(occurrence).unwrap_or(1),
                    signature: symbol.to_string(),
                    parent: None,
                    confidence: CodeIntelConfidence::Scip,
                    source: "scip".to_string(),
                });
            }
        }
    }
    dedupe_symbols(&mut snapshot.symbols);
    dedupe_edges(&mut snapshot.edges);
    Ok(snapshot)
}

fn scip_is_definition(occurrence: &Value) -> bool {
    occurrence
        .get("symbol_roles")
        .or_else(|| occurrence.get("symbolRoles"))
        .and_then(Value::as_u64)
        .map(|roles| roles & 1 == 1)
        .unwrap_or(false)
}

fn scip_start_line(occurrence: &Value) -> Option<usize> {
    occurrence
        .get("range")
        .and_then(Value::as_array)?
        .first()?
        .as_u64()
        .map(|line| line as usize + 1)
}

fn language_for_path(path: &str) -> Option<String> {
    let ext = Path::new(path).extension()?.to_str()?;
    let language = match ext {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        _ => return None,
    };
    Some(language.to_string())
}

fn dedupe_symbols(symbols: &mut Vec<CodeIntelSymbol>) {
    let mut seen = HashSet::new();
    symbols.retain(|symbol| seen.insert(symbol.id.clone()));
}

fn dedupe_edges(edges: &mut Vec<CodeIntelEdge>) {
    let mut seen = HashSet::new();
    edges.retain(|edge| {
        seen.insert((
            edge.from.clone(),
            edge.to.clone(),
            edge.kind,
            edge.confidence,
        ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    #[test]
    fn tree_sitter_adapter_extracts_defs_and_calls() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("src/auth.rs"),
            "pub fn login() {}\npub fn run() { login(); }\n",
        );
        let mut manifest = CodeIntelManifest::default();
        manifest.files.insert(
            "src/auth.rs".to_string(),
            CodeIntelFile {
                path: "src/auth.rs".to_string(),
                language: Some("rust".to_string()),
            },
        );
        let snapshot = TreeSitterAdapter.index(dir.path(), &manifest).unwrap();
        assert!(snapshot.symbols.iter().any(|symbol| symbol.name == "login"));
        assert!(
            snapshot
                .edges
                .iter()
                .any(|edge| edge.kind == CodeIntelEdgeKind::Definition)
        );
    }

    #[test]
    fn textual_call_graph_skips_low_signal_calls() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("src/a.rs"),
            "pub fn new() {}\npub fn login() {}\n",
        );
        write(
            &dir.path().join("src/b.rs"),
            "pub fn run() { new(); login(); }\n",
        );
        let mut manifest = CodeIntelManifest::default();
        for path in ["src/a.rs", "src/b.rs"] {
            manifest.files.insert(
                path.to_string(),
                CodeIntelFile {
                    path: path.to_string(),
                    language: Some("rust".to_string()),
                },
            );
        }
        let snapshot = TreeSitterAdapter.index(dir.path(), &manifest).unwrap();
        assert!(snapshot.edges.iter().any(|edge| {
            edge.kind == CodeIntelEdgeKind::Call && edge.from == "src/b.rs" && edge.to == "src/a.rs"
        }));
        assert!(!snapshot.edges.iter().any(|edge| {
            edge.kind == CodeIntelEdgeKind::Reference && edge.to.contains(":new:")
        }));
    }

    #[test]
    fn rust_analyzer_unavailable_falls_back_without_failing() {
        let dir = TempDir::new().unwrap();
        let manifest = CodeIntelManifest::default();
        let snapshot = RustAnalyzerAdapter::default()
            .index(dir.path(), &manifest)
            .unwrap();
        assert_eq!(snapshot.adapter, "rust_analyzer");
    }

    #[test]
    fn scip_adapter_reads_json_index() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("src/lib.rs"), "pub fn login() {}\n");
        write(
            &dir.path().join(".scip.json"),
            r#"{"documents":[{"relative_path":"src/lib.rs","occurrences":[{"symbol":"local 0 login().","symbol_roles":1,"range":[0,0,0,3]}]}]}"#,
        );
        let snapshot = ScipAdapter
            .index(dir.path(), &CodeIntelManifest::default())
            .unwrap();
        assert!(
            snapshot
                .edges
                .iter()
                .any(|edge| edge.confidence == CodeIntelConfidence::Scip)
        );
    }
}
