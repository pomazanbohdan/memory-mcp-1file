use std::path::Path;

use crate::types::{ChunkType, CodeChunk, Language};

use super::parser::languages::get_language_support;
use super::scanner::detect_language;

const MAX_CHUNK_CHARS: usize = 4000;
const MIN_CHUNK_CHARS: usize = 10;
const MAX_CHUNK_LINES: usize = 150;
const MIN_OTHER_CHUNK_LINES: usize = 3;

pub fn chunk_file(path: &Path, content: &str, project_id: &str) -> Vec<CodeChunk> {
    let language = detect_language(path);
    let file_path = path.to_string_lossy().to_string();

    if content.trim().is_empty() {
        return vec![];
    }

    if let Some(support) = get_language_support(language.clone()) {
        chunk_by_ast(
            content,
            &file_path,
            project_id,
            language,
            support.get_language(),
        )
    } else {
        chunk_by_structure(content, &file_path, project_id, language)
    }
}

fn chunk_by_ast(
    content: &str,
    file_path: &str,
    project_id: &str,
    language: Language,
    ts_language: tree_sitter::Language,
) -> Vec<CodeChunk> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&ts_language).is_err() {
        return chunk_by_structure(content, file_path, project_id, language);
    }

    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => return chunk_by_structure(content, file_path, project_id, language),
    };

    let mut chunks = Vec::new();
    let root = tree.root_node();
    let source = content.as_bytes();

    // Iteratively walk the AST to find chunk-worthy nodes at any depth
    walk_ast_iterative(
        root,
        source,
        content,
        file_path,
        project_id,
        &language,
        &mut chunks,
    );

    if chunks.is_empty() {
        return chunk_by_structure(content, file_path, project_id, language);
    }

    chunks
}

/// Iteratively walk the AST tree, chunking definitions at any nesting depth.
/// Builds hierarchical context_path (breadcrumbs) for each chunk without recursive calls.
fn walk_ast_iterative(
    root: tree_sitter::Node,
    source: &[u8],
    content: &str,
    file_path: &str,
    project_id: &str,
    language: &Language,
    chunks: &mut Vec<CodeChunk>,
) {
    let mut stack = vec![root];

    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            let start_byte = child.start_byte();
            let end_byte = child.end_byte();
            let node_text = content.get(start_byte..end_byte).unwrap_or("");

            if node_text.len() < MIN_CHUNK_CHARS {
                continue;
            }

            let chunk_type = detect_chunk_type(&child);
            let is_named_scope = is_scope_node(child.kind());

            if is_chunk_worthy(&child, chunk_type.clone()) {
                let context_path = build_context_path(child, source);

                if node_text.len() <= MAX_CHUNK_CHARS {
                    let line_count = child.end_position().row - child.start_position().row + 1;
                    if chunk_type == ChunkType::Other && line_count < MIN_OTHER_CHUNK_LINES {
                        // Skip noise, but still recurse into named scopes
                        if is_named_scope {
                            stack.push(child);
                        }
                        continue;
                    }
                    chunks.push(create_chunk(
                        node_text,
                        file_path,
                        project_id,
                        language.clone(),
                        child.start_position().row as u32 + 1,
                        child.end_position().row as u32 + 1,
                        chunk_type,
                        context_path,
                    ));
                } else {
                    let sub_chunks = split_large_node(
                        node_text,
                        file_path,
                        project_id,
                        language.clone(),
                        child.start_position().row as u32 + 1,
                        context_path,
                    );
                    chunks.extend(sub_chunks);
                }

                // Also recurse into this node to find nested definitions
                // (e.g. methods inside impl blocks, nested classes)
                if is_named_scope {
                    stack.push(child);
                }
            } else if is_named_scope {
                // Not chunk-worthy itself, but may contain chunk-worthy children
                stack.push(child);
            }
        }
    }
}

/// Check if a node is chunk-worthy (definition that should become a chunk)
fn is_chunk_worthy(node: &tree_sitter::Node, chunk_type: ChunkType) -> bool {
    // Top-level items and named definitions are always chunk-worthy
    chunk_type != ChunkType::Other || node.parent().is_none_or(|p| p.parent().is_none())
}

/// Check if a node kind represents a named scope that may contain nested definitions.
fn is_scope_node(kind: &str) -> bool {
    matches!(
        kind,
        "impl_item"
            | "trait_item"
            | "mod_item"
            | "module"
            | "class_definition"
            | "class_declaration"
            | "class_body"
            | "declaration_list"
            | "block"
            | "namespace_definition"
            | "interface_declaration"
            | "enum_item"
    )
}

/// Build a hierarchical context path (breadcrumbs) by walking UP from a node
/// to the root, collecting named scope ancestors.
///
/// Example output: "mod:codebase > impl:ChunkerService > fn:process_file"
fn build_context_path(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut parts = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        if is_named_scope_for_path(parent.kind()) {
            if let Some(name) = extract_scope_name(parent, source) {
                parts.push(format!("{}:{}", simplify_kind(parent.kind()), name));
            }
        }
        current = parent.parent();
    }

    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    Some(parts.join(" > "))
}

/// Node kinds that contribute to context_path breadcrumbs.
fn is_named_scope_for_path(kind: &str) -> bool {
    matches!(
        kind,
        "mod_item"
            | "impl_item"
            | "trait_item"
            | "class_definition"
            | "class_declaration"
            | "module"
            | "namespace_definition"
            | "interface_declaration"
            | "function_item"
            | "function_definition"
            | "method_definition"
    )
}

/// Simplify tree-sitter node kind to a short label for breadcrumbs.
fn simplify_kind(kind: &str) -> &str {
    match kind {
        "mod_item" | "module" => "mod",
        "impl_item" => "impl",
        "trait_item" => "trait",
        "class_definition" | "class_declaration" => "class",
        "function_item" | "function_definition" | "method_definition" => "fn",
        "namespace_definition" => "ns",
        "interface_declaration" => "iface",
        _ => kind,
    }
}

/// Extract the name of a scope node from its AST children.
/// Tries `child_by_field_name("name")` first, then specific patterns.
fn extract_scope_name(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    // Try the standard "name" field first (works for most languages)
    if let Some(name_node) = node.child_by_field_name("name") {
        return source
            .get(name_node.byte_range())
            .and_then(|b| std::str::from_utf8(b).ok())
            .map(|s| s.to_string());
    }

    // For Rust impl blocks: `impl Type { ... }` — look for type child
    if node.kind() == "impl_item" {
        if let Some(type_node) = node.child_by_field_name("type") {
            return source
                .get(type_node.byte_range())
                .and_then(|b| std::str::from_utf8(b).ok())
                .map(|s| s.to_string());
        }
    }

    // Fallback: find first identifier/type_identifier child
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" || child.kind() == "type_identifier" {
            return source
                .get(child.byte_range())
                .and_then(|b| std::str::from_utf8(b).ok())
                .map(|s| s.to_string());
        }
    }

    None
}

fn chunk_by_structure(
    content: &str,
    file_path: &str,
    project_id: &str,
    language: Language,
) -> Vec<CodeChunk> {
    let paragraphs: Vec<&str> = content.split("\n\n").collect();
    let mut chunks = Vec::new();
    let mut current_chunk = String::new();
    let mut current_start_line: u32 = 1;
    let mut line_counter: u32 = 1;

    for para in paragraphs {
        let para_lines = para.lines().count() as u32;

        if current_chunk.len() + para.len() > MAX_CHUNK_CHARS && !current_chunk.is_empty() {
            let end_line = line_counter.saturating_sub(1);
            chunks.push(create_chunk(
                &current_chunk,
                file_path,
                project_id,
                language.clone(),
                current_start_line,
                end_line,
                ChunkType::Other,
                None, // no context_path for structure-based chunking
            ));
            current_chunk.clear();
            current_start_line = line_counter;
        }

        if !current_chunk.is_empty() {
            current_chunk.push_str("\n\n");
        }
        current_chunk.push_str(para);
        line_counter += para_lines + 1;
    }

    if current_chunk.len() >= MIN_CHUNK_CHARS {
        chunks.push(create_chunk(
            &current_chunk,
            file_path,
            project_id,
            language,
            current_start_line,
            line_counter,
            ChunkType::Other,
            None, // no context_path for structure-based chunking
        ));
    }

    chunks
}

fn split_large_node(
    text: &str,
    file_path: &str,
    project_id: &str,
    language: Language,
    base_line: u32,
    context_path: Option<String>,
) -> Vec<CodeChunk> {
    let lines: Vec<&str> = text.lines().collect();
    let mut chunks = Vec::new();
    let mut current_start = 0;

    while current_start < lines.len() {
        let end = (current_start + MAX_CHUNK_LINES).min(lines.len());
        let chunk_lines = &lines[current_start..end];
        let chunk_content = chunk_lines.join("\n");

        if chunk_content.len() >= MIN_CHUNK_CHARS {
            chunks.push(create_chunk(
                &chunk_content,
                file_path,
                project_id,
                language.clone(),
                base_line + current_start as u32,
                base_line + end as u32,
                ChunkType::Other,
                context_path.clone(), // propagate parent context_path to sub-chunks
            ));
        }

        current_start = end;
    }

    chunks
}

#[allow(clippy::too_many_arguments)]
fn create_chunk(
    content: &str,
    file_path: &str,
    project_id: &str,
    language: Language,
    start_line: u32,
    end_line: u32,
    chunk_type: ChunkType,
    context_path: Option<String>,
) -> CodeChunk {
    let content_hash = blake3::hash(content.as_bytes()).to_hex().to_string();

    CodeChunk {
        id: None,
        file_path: file_path.to_string(),
        content: content.to_string(),
        language,
        start_line,
        end_line,
        chunk_type,
        name: None,
        context_path,
        embedding: None,
        content_hash,
        project_id: Some(project_id.to_string()),
        indexed_at: crate::types::Datetime::default(),
    }
}

fn detect_chunk_type(node: &tree_sitter::Node) -> ChunkType {
    match node.kind() {
        "function_item" | "function_definition" | "function_declaration" | "method_definition" => {
            ChunkType::Function
        }
        "struct_item" | "class_definition" | "class_declaration" => ChunkType::Class,
        "impl_item" | "trait_item" | "interface_declaration" => ChunkType::Class,
        "mod_item" | "module" => ChunkType::Module,
        _ => ChunkType::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // -----------------------------------------------------------------------
    // Helper: chunk_by_structure is private, so we drive tests through the
    // public chunk_file() entry point using paths with no known language
    // support (plain text files) so we always get the structure-based path.
    // -----------------------------------------------------------------------

    fn plain_path() -> &'static Path {
        Path::new("test.txt")
    }

    fn rust_path() -> &'static Path {
        Path::new("test.rs")
    }

    // -----------------------------------------------------------------------
    // Empty / trivial input
    // -----------------------------------------------------------------------

    #[test]
    fn test_empty_content_yields_no_chunks() {
        let chunks = chunk_file(plain_path(), "", "proj");
        assert!(chunks.is_empty(), "Empty content should produce no chunks");
    }

    #[test]
    fn test_whitespace_only_yields_no_chunks() {
        let chunks = chunk_file(plain_path(), "   \n\t\n  ", "proj");
        assert!(
            chunks.is_empty(),
            "Whitespace-only content should produce no chunks"
        );
    }

    // -----------------------------------------------------------------------
    // MIN_CHUNK_CHARS enforcement
    // -----------------------------------------------------------------------

    #[test]
    fn test_content_below_min_chars_yields_no_chunk() {
        // MIN_CHUNK_CHARS = 10; 9 non-whitespace chars should not be emitted
        let short = "123456789"; // 9 chars
        let chunks = chunk_file(plain_path(), short, "proj");
        assert!(
            chunks.is_empty(),
            "Content shorter than MIN_CHUNK_CHARS should not produce a chunk"
        );
    }

    #[test]
    fn test_content_at_min_chars_yields_one_chunk() {
        let exactly_min = "1234567890"; // exactly 10 chars = MIN_CHUNK_CHARS
        let chunks = chunk_file(plain_path(), exactly_min, "proj");
        assert_eq!(
            chunks.len(),
            1,
            "Content at exactly MIN_CHUNK_CHARS should produce 1 chunk"
        );
    }

    // -----------------------------------------------------------------------
    // MAX_CHUNK_CHARS enforcement — structure chunker splits on paragraphs
    // -----------------------------------------------------------------------

    #[test]
    fn test_large_content_split_into_multiple_chunks() {
        // Build a text larger than MAX_CHUNK_CHARS (4000 chars).
        // Use double-newlines so chunk_by_structure sees paragraph boundaries.
        let paragraph = "x".repeat(1500); // 1500 chars per paragraph
        let content = format!("{paragraph}\n\n{paragraph}\n\n{paragraph}"); // ~4502 chars

        let chunks = chunk_file(plain_path(), &content, "proj");
        assert!(
            chunks.len() >= 2,
            "Content > MAX_CHUNK_CHARS should be split into multiple chunks, got {}",
            chunks.len()
        );
        for chunk in &chunks {
            assert!(
                chunk.content.len() <= MAX_CHUNK_CHARS,
                "Every chunk must be <= MAX_CHUNK_CHARS chars, got {}",
                chunk.content.len()
            );
        }
    }

    // -----------------------------------------------------------------------
    // AST chunker: MIN_OTHER_CHUNK_LINES for ChunkType::Other nodes
    // -----------------------------------------------------------------------

    #[test]
    fn test_ast_skips_small_other_chunks_but_keeps_functions() {
        // A Rust file with a real function (kept) and a tiny top-level expression
        // that would be ChunkType::Other with < MIN_OTHER_CHUNK_LINES (3).
        let src = r#"
use std::io;

fn hello() {
    println!("hello world");
    println!("line two");
    println!("line three");
}
"#;
        let chunks = chunk_file(rust_path(), src, "proj");

        // The function should appear
        let has_function = chunks.iter().any(|c| c.chunk_type == ChunkType::Function);
        assert!(has_function, "AST chunker should keep function chunks");

        // No chunk should be a tiny Other (< MIN_OTHER_CHUNK_LINES lines)
        for chunk in &chunks {
            if chunk.chunk_type == ChunkType::Other {
                let line_count = chunk.end_line.saturating_sub(chunk.start_line) + 1;
                assert!(
                    line_count >= MIN_OTHER_CHUNK_LINES as u32,
                    "Other chunks with < {MIN_OTHER_CHUNK_LINES} lines should be skipped, \
                     found one with {line_count} lines: {:?}",
                    chunk.content
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // split_large_node: large node is sub-divided into MAX_CHUNK_LINES slices
    // -----------------------------------------------------------------------

    #[test]
    fn test_large_node_subdivided_within_line_limit() {
        // Build a Rust function body large enough to exceed both MAX_CHUNK_CHARS
        // (4000 chars) and MAX_CHUNK_LINES (150 lines) to trigger split_large_node.
        // Each statement is ~40 chars → 200 lines × 40 chars = ~8000 chars > 4000.
        let body: String = (0..200)
            .map(|i| format!("    let _variable_{i:04} = {i} * {i} + {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let src = format!("fn big() {{\n{body}\n}}\n");

        // Confirm the source is actually large enough to trigger splitting
        assert!(
            src.len() > MAX_CHUNK_CHARS,
            "Test source ({} chars) must exceed MAX_CHUNK_CHARS ({MAX_CHUNK_CHARS})",
            src.len()
        );

        let chunks = chunk_file(rust_path(), &src, "proj");

        assert!(
            chunks.len() >= 2,
            "Function with >MAX_CHUNK_LINES lines should be split into >= 2 chunks, got {}",
            chunks.len()
        );
        for chunk in &chunks {
            let line_count = (chunk.end_line - chunk.start_line + 1) as usize;
            assert!(
                line_count <= MAX_CHUNK_LINES + 2, // small tolerance for header/footer lines
                "Chunk has {line_count} lines which exceeds MAX_CHUNK_LINES ({MAX_CHUNK_LINES})"
            );
        }
    }

    // -----------------------------------------------------------------------
    // chunk metadata: project_id, file_path, language
    // -----------------------------------------------------------------------

    #[test]
    fn test_chunk_metadata_is_correct() {
        let content = "fn foo() { let x = 1 + 2 + 3; }";
        let chunks = chunk_file(rust_path(), content, "my_project");

        assert!(!chunks.is_empty(), "Should produce at least one chunk");
        for chunk in &chunks {
            assert_eq!(
                chunk.project_id.as_deref(),
                Some("my_project"),
                "project_id mismatch"
            );
            assert!(
                chunk.file_path.contains("test.rs"),
                "file_path should contain 'test.rs'"
            );
            assert!(
                chunk.start_line >= 1,
                "start_line should be 1-based, got {}",
                chunk.start_line
            );
        }
    }
}
