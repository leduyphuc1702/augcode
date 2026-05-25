use crate::{Delta, FileEntry, Manifest, SymbolDefinition, sha256_hex};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

const FALLBACK_CHUNK_LINES: usize = 80;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AstChunk {
    pub path: String,
    pub kind: String,
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default)]
    pub signature: String,
    pub text_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AstIndex {
    pub chunks: Vec<AstChunk>,
}

#[derive(Debug, Clone, Default)]
struct AstFileAnalysis {
    chunks: Vec<AstChunk>,
    symbols: Vec<SymbolDefinition>,
    import_paths: Vec<String>,
}

impl AstIndex {
    pub fn rebuild(root: &Path, manifest: &Manifest) -> Result<Self> {
        let mut index = Self::default();
        for entry in manifest.files.values() {
            index.upsert_file(root, entry)?;
        }
        Ok(index)
    }

    pub fn apply_delta(&mut self, root: &Path, delta: &Delta) -> Result<()> {
        for path in &delta.removed {
            self.remove_path(path);
        }
        for rename in &delta.renamed {
            self.remove_path(&rename.from);
        }
        for entry in delta.added.iter().chain(delta.modified.iter()) {
            self.upsert_file(root, entry)?;
        }
        for rename in &delta.renamed {
            self.upsert_file(root, &rename.entry)?;
        }
        Ok(())
    }

    pub fn upsert_file(&mut self, root: &Path, entry: &FileEntry) -> Result<()> {
        let text = fs::read_to_string(root.join(&entry.path))?;
        self.upsert_text(entry, &text);
        Ok(())
    }

    pub fn upsert_text(&mut self, entry: &FileEntry, text: &str) {
        self.remove_path(&entry.path);
        let chunks = analyze_file(entry, text)
            .map(|analysis| analysis.chunks)
            .unwrap_or_else(|| fallback_chunks(entry, text));
        self.chunks.extend(chunks);
    }

    pub fn remove_path(&mut self, path: &str) {
        self.chunks.retain(|chunk| chunk.path != path);
    }

    pub fn chunk_for_line(&self, path: &str, line: usize) -> Option<&AstChunk> {
        self.chunks
            .iter()
            .filter(|chunk| chunk.path == path)
            .filter(|chunk| chunk.start_line <= line && line <= chunk.end_line)
            .min_by_key(|chunk| chunk.end_line.saturating_sub(chunk.start_line))
    }

    pub fn chunks_for_path(&self, path: &str) -> impl Iterator<Item = &AstChunk> {
        self.chunks.iter().filter(move |chunk| chunk.path == path)
    }
}

pub fn extract_symbols(entry: &FileEntry, text: &str) -> Option<Vec<SymbolDefinition>> {
    analyze_file(entry, text).map(|analysis| analysis.symbols)
}

pub fn extract_import_paths(entry: &FileEntry, text: &str) -> Option<Vec<String>> {
    analyze_file(entry, text).map(|analysis| analysis.import_paths)
}

fn analyze_file(entry: &FileEntry, text: &str) -> Option<AstFileAnalysis> {
    let (tree, _language) = parse_tree(entry, text)?;
    let mut analysis = AstFileAnalysis::default();
    let bytes = text.as_bytes();
    let root = tree.root_node();
    let mut parent_stack = Vec::new();
    collect_nodes(entry, bytes, root, &mut parent_stack, &mut analysis);
    if analysis.chunks.is_empty() {
        analysis.chunks = fallback_chunks(entry, text);
    }
    Some(analysis)
}

fn parse_tree(entry: &FileEntry, text: &str) -> Option<(tree_sitter::Tree, &'static str)> {
    let mut parser = Parser::new();
    let language_name = set_parser_language(&mut parser, entry.language.as_deref())?;
    let tree = parser.parse(text, None)?;
    Some((tree, language_name))
}

fn set_parser_language(parser: &mut Parser, language: Option<&str>) -> Option<&'static str> {
    let (tree_language, language_name): (Language, &'static str) = match language? {
        "rust" => (tree_sitter_rust::LANGUAGE.into(), "rust"),
        "typescript" => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
        ),
        "typescriptreact" => (
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            "typescriptreact",
        ),
        "javascript" | "javascriptreact" => (tree_sitter_javascript::LANGUAGE.into(), "javascript"),
        "python" => (tree_sitter_python::LANGUAGE.into(), "python"),
        _ => return None,
    };
    parser.set_language(&tree_language).ok()?;
    Some(language_name)
}

fn collect_nodes(
    entry: &FileEntry,
    bytes: &[u8],
    node: Node<'_>,
    parent_stack: &mut Vec<String>,
    analysis: &mut AstFileAnalysis,
) {
    if let Some(import_path) = import_path_for_node(bytes, node) {
        analysis.import_paths.push(import_path);
    }

    let symbol = symbol_for_node(entry, bytes, node, parent_stack.last().cloned());
    let pushed_parent = if let Some((chunk, symbol)) = symbol {
        let parent_label = symbol_parent_label(&chunk);
        analysis.symbols.push(symbol);
        analysis.chunks.push(chunk);
        if is_container_kind(node.kind()) {
            parent_stack.push(parent_label);
            true
        } else {
            false
        }
    } else {
        false
    };

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_nodes(entry, bytes, child, parent_stack, analysis);
    }

    if pushed_parent {
        parent_stack.pop();
    }
}

fn symbol_for_node(
    entry: &FileEntry,
    bytes: &[u8],
    node: Node<'_>,
    parent: Option<String>,
) -> Option<(AstChunk, SymbolDefinition)> {
    let kind = symbol_kind(node.kind())?;
    if node.kind() == "lexical_declaration" && !contains_function_node(node) {
        return None;
    }
    let name = symbol_name_for_node(bytes, node, kind)?;
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row.max(node.start_position().row) + 1;
    let text = node.utf8_text(bytes).ok().unwrap_or_default();
    let signature = signature_from_text(text);
    let text_hash = format!("sha256:{}", sha256_hex(text.as_bytes()));
    let language = entry.language.clone();
    let chunk = AstChunk {
        path: entry.path.clone(),
        kind: kind.to_string(),
        name: name.clone(),
        start_line,
        end_line,
        parent: parent.clone(),
        signature: signature.clone(),
        text_hash,
        language: language.clone(),
    };
    let symbol = SymbolDefinition {
        path: entry.path.clone(),
        name,
        kind: kind.to_string(),
        start_line,
        end_line,
        signature,
        parent,
        language,
        confidence: 100,
    };
    Some((chunk, symbol))
}

fn contains_function_node(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "arrow_function" | "function" | "function_declaration"
        ) || contains_function_node(child)
        {
            return true;
        }
    }
    false
}

fn symbol_kind(node_kind: &str) -> Option<&'static str> {
    match node_kind {
        "function_item" | "function_declaration" | "function_definition" => Some("function"),
        "method_definition" => Some("method"),
        "struct_item" => Some("struct"),
        "enum_item" => Some("enum"),
        "trait_item" => Some("trait"),
        "impl_item" => Some("impl"),
        "class_declaration" | "class_definition" => Some("class"),
        "interface_declaration" => Some("interface"),
        "type_alias_declaration" => Some("type"),
        "lexical_declaration" | "variable_declarator" => Some("function"),
        _ => None,
    }
}

fn is_container_kind(node_kind: &str) -> bool {
    matches!(
        node_kind,
        "impl_item"
            | "trait_item"
            | "class_declaration"
            | "class_definition"
            | "interface_declaration"
    )
}

fn symbol_name_for_node(bytes: &[u8], node: Node<'_>, kind: &str) -> Option<String> {
    if kind == "impl" {
        return impl_name(bytes, node);
    }
    if node.kind() == "lexical_declaration" {
        return first_variable_name(bytes, node);
    }
    if let Some(name) = node.child_by_field_name("name") {
        return clean_name(name.utf8_text(bytes).ok()?);
    }
    first_identifier_name(bytes, node)
}

fn impl_name(bytes: &[u8], node: Node<'_>) -> Option<String> {
    if let Some(type_node) = node.child_by_field_name("type") {
        return clean_name(type_node.utf8_text(bytes).ok()?);
    }
    let text = node.utf8_text(bytes).ok()?;
    let first_line = text.lines().next()?.trim();
    let after_impl = first_line.strip_prefix("impl")?.trim();
    let name = after_impl
        .split(['<', '{', ' ', '\n', '\t'])
        .find(|part| !part.is_empty())?;
    clean_name(name)
}

fn first_variable_name(bytes: &[u8], node: Node<'_>) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator"
            && let Some(name) = child.child_by_field_name("name")
        {
            return clean_name(name.utf8_text(bytes).ok()?);
        }
    }
    None
}

fn first_identifier_name(bytes: &[u8], node: Node<'_>) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier" || child.kind() == "type_identifier" {
            return clean_name(child.utf8_text(bytes).ok()?);
        }
    }
    None
}

fn clean_name(raw: &str) -> Option<String> {
    let name = raw
        .trim()
        .trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-');
    (!name.is_empty()).then(|| name.to_string())
}

fn signature_from_text(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(240)
        .collect()
}

fn symbol_parent_label(chunk: &AstChunk) -> String {
    format!("{}:{}:{}", chunk.kind, chunk.name, chunk.start_line)
}

fn import_path_for_node(bytes: &[u8], node: Node<'_>) -> Option<String> {
    match node.kind() {
        "mod_item" => node
            .child_by_field_name("name")
            .and_then(|name| clean_name(name.utf8_text(bytes).ok()?)),
        "import_statement" | "export_statement" => first_string_literal(bytes, node),
        "import_from_statement" => {
            first_dotted_import(bytes, node).or_else(|| first_string_literal(bytes, node))
        }
        _ => None,
    }
}

fn first_string_literal(bytes: &[u8], node: Node<'_>) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "string" | "string_fragment") {
            let text = child.utf8_text(bytes).ok()?.trim_matches(['"', '\'', '`']);
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
        if let Some(value) = first_string_literal(bytes, child) {
            return Some(value);
        }
    }
    None
}

fn first_dotted_import(bytes: &[u8], node: Node<'_>) -> Option<String> {
    let text = node.utf8_text(bytes).ok()?;
    let after_from = text.trim().strip_prefix("from ")?;
    let module = after_from.split_whitespace().next()?;
    Some(module.trim_matches('.').replace('.', "/"))
}

fn fallback_chunks(entry: &FileEntry, text: &str) -> Vec<AstChunk> {
    let mut chunks = Vec::new();
    let lines: Vec<_> = text.lines().collect();
    if lines.is_empty() {
        chunks.push(fallback_chunk(entry, 1, 1, ""));
        return chunks;
    }
    for (index, slice) in lines.chunks(FALLBACK_CHUNK_LINES).enumerate() {
        let start = index * FALLBACK_CHUNK_LINES + 1;
        let end = start + slice.len() - 1;
        chunks.push(fallback_chunk(entry, start, end, &slice.join("\n")));
    }
    chunks
}

fn fallback_chunk(entry: &FileEntry, start_line: usize, end_line: usize, text: &str) -> AstChunk {
    AstChunk {
        path: entry.path.clone(),
        kind: "file_chunk".to_string(),
        name: entry.path.clone(),
        start_line,
        end_line,
        parent: None,
        signature: format!("{} lines {}-{}", entry.path, start_line, end_line),
        text_hash: format!("sha256:{}", sha256_hex(text.as_bytes())),
        language: entry.language.clone(),
    }
}

pub fn containment_edges(chunks: &[AstChunk]) -> Vec<(String, String)> {
    let by_path = chunks.iter().fold(
        BTreeMap::<String, Vec<&AstChunk>>::new(),
        |mut acc, chunk| {
            acc.entry(chunk.path.clone()).or_default().push(chunk);
            acc
        },
    );
    by_path
        .into_iter()
        .flat_map(|(path, chunks)| {
            chunks
                .into_iter()
                .filter(|chunk| chunk.kind != "file_chunk")
                .map(move |chunk| (path.clone(), symbol_id(chunk)))
        })
        .collect()
}

pub fn symbol_id(chunk: &AstChunk) -> String {
    format!(
        "{}:{}:{}:{}",
        chunk.path, chunk.kind, chunk.name, chunk.start_line
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            content_hash: "sha256:test".to_string(),
            size_bytes: 0,
            language: crate::language_for_path(Path::new(path)),
            is_generated: false,
        }
    }

    #[test]
    fn parses_rust_symbols() {
        let entry = entry("src/lib.rs");
        let symbols = extract_symbols(
            &entry,
            "pub struct Engine;\nimpl Engine {\n  pub fn run(&self) {}\n}\npub trait Tool {}\n",
        )
        .unwrap();
        assert!(symbols.iter().any(|symbol| symbol.name == "Engine"));
        assert!(symbols.iter().any(|symbol| symbol.name == "run"));
        assert!(symbols.iter().any(|symbol| symbol.kind == "trait"));
    }

    #[test]
    fn parses_typescript_symbols() {
        let entry = entry("src/app.ts");
        let symbols = extract_symbols(
            &entry,
            "export interface User {}\nexport class Login {}\nexport function auth() {}\n",
        )
        .unwrap();
        assert!(symbols.iter().any(|symbol| symbol.name == "User"));
        assert!(symbols.iter().any(|symbol| symbol.name == "Login"));
        assert!(symbols.iter().any(|symbol| symbol.name == "auth"));
    }

    #[test]
    fn parses_python_symbols() {
        let entry = entry("app.py");
        let symbols = extract_symbols(
            &entry,
            "class Auth:\n    async def login(self):\n        pass\n\ndef validate():\n    pass\n",
        )
        .unwrap();
        assert!(symbols.iter().any(|symbol| symbol.name == "Auth"));
        assert!(symbols.iter().any(|symbol| symbol.name == "login"));
        assert!(symbols.iter().any(|symbol| symbol.name == "validate"));
    }
}
