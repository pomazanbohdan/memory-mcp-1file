//! In-memory BM25 search engine for code chunks.
//!
//! Uses the `bm-25` crate with a custom `CodeTokenizer` that handles:
//! - Splitting by non-alphanumeric characters (snake_case, kebab-case)
//! - Splitting on camelCase / PascalCase transitions
//! - Emitting the full original token alongside sub-tokens (for exact-match boost)

use bm_25::{Document, SearchEngineBuilder, Tokenizer};
use std::collections::HashMap;
use tokio::sync::RwLock;

use crate::storage::StorageBackend;
use crate::types::ScoredCodeChunk;

// ─── Tokenizer ───────────────────────────────────────────────────────────────

/// Code-aware tokenizer that handles camelCase, PascalCase, snake_case, etc.
#[derive(Debug, Clone, Default)]
pub struct CodeTokenizer;

impl Tokenizer for CodeTokenizer {
    fn tokenize<'a>(&'a self, input_text: &'a str) -> impl Iterator<Item = String> + 'a {
        CodeTokenIter::new(input_text)
    }
}

struct CodeTokenIter<'a> {
    input: &'a str,
    /// Words already collected and ready to emit
    pending: std::collections::VecDeque<String>,
    /// Whether we have started processing
    done: bool,
}

impl<'a> CodeTokenIter<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            pending: std::collections::VecDeque::new(),
            done: false,
        }
    }

    fn fill_pending(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        tokenize_code(self.input, &mut self.pending);
    }
}

impl<'a> Iterator for CodeTokenIter<'a> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        self.fill_pending();
        self.pending.pop_front()
    }
}

/// Core tokenization logic.
/// For each "word" segment (split by non-alphanumeric boundaries) we:
///  1. Emit the full lowercased word (for exact-match)
///  2. Emit sub-tokens produced by camelCase/digit transitions
fn tokenize_code(text: &str, out: &mut std::collections::VecDeque<String>) {
    // Split on runs of non-alphanumeric characters
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }

        let word_lower = word.to_lowercase();

        // Collect sub-tokens from camelCase splitting
        let sub_tokens = split_camel(word);

        // Emit sub-tokens first (highest recall value)
        let mut has_subtokens = false;
        for t in &sub_tokens {
            if !t.is_empty() {
                out.push_back(t.clone());
                has_subtokens = true;
            }
        }

        // Emit full lowercased word (for exact-match queries like "OdooAuthService")
        // Only add it if it differs from the single sub-token (avoids duplication when no split)
        if !has_subtokens
            || sub_tokens.len() > 1
            || sub_tokens.first().map(|s| s.as_str()) != Some(&word_lower)
        {
            out.push_back(word_lower);
        }
    }
}

/// Split a word on camelCase / PascalCase / digit transitions.
/// Returns lowercase sub-tokens.
///
/// Examples:
///   "OdooAuthService" → ["odoo", "auth", "service"]
///   "parseXMLContent" → ["parse", "xml", "content"]
///   "HTML5Parser"     → ["html5", "parser"]  (digit stays with preceding segment)
fn split_camel(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let n = chars.len();
    if n == 0 {
        return vec![];
    }

    let mut tokens = Vec::new();
    let mut start = 0;

    // Character class: 0=other, 1=lower, 2=upper, 3=digit
    let class = |c: char| -> u8 {
        if c.is_lowercase() {
            1
        } else if c.is_uppercase() {
            2
        } else if c.is_ascii_digit() {
            3
        } else {
            0
        }
    };

    let mut i = 1;
    while i < n {
        let prev = class(chars[i - 1]);
        let cur = class(chars[i]);
        let next = if i + 1 < n { class(chars[i + 1]) } else { 0 };

        let split = match (prev, cur) {
            // lower → upper: "camelCase" → split before 'C'
            (1, 2) => true,
            // lower → digit: "version2" → keep together (don't split)
            (1, 3) => false,
            // digit → lower: "3d" or "html5parser" → split before lower
            (3, 1) => true,
            // digit → upper: "3D" → split
            (3, 2) => true,
            // upper → upper followed by lower: "HTMLParser" → split before second-to-last upper
            // "..XP" where next='a' → split at 'X' (produces "HTML", "Parser")
            (2, 2) if next == 1 => true,
            _ => false,
        };

        if split {
            let segment: String = chars[start..i].iter().collect();
            let seg_lower = segment.to_lowercase();
            if !seg_lower.is_empty() {
                tokens.push(seg_lower);
            }
            start = i;
        }
        i += 1;
    }

    // Last segment
    let segment: String = chars[start..].iter().collect();
    let seg_lower = segment.to_lowercase();
    if !seg_lower.is_empty() {
        tokens.push(seg_lower);
    }

    tokens
}

// ─── Search Engine ────────────────────────────────────────────────────────────

/// A per-project BM25 index over code chunks.
///
/// Keyed by chunk string ID. The document contents are the chunk's full
/// `content` field combined with `file_path` and optional `name` so that
/// symbol names (function names, class names) are searchable.
type InnerEngine = bm_25::SearchEngine<String, u32, CodeTokenizer>;

/// Metadata stored alongside the BM25 index so we can reconstruct
/// `ScoredCodeChunk` results without keeping all chunk content in RAM.
///
/// **`content` is intentionally absent** — storing 50 000 × ~2 KB strings in
/// the in-memory index would cause an OOM. Instead, `search()` fetches the
/// content for only the top-N results from the database on demand.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ChunkMeta {
    pub id: String,
    pub file_path: String,
    pub language: crate::types::Language,
    pub start_line: u32,
    pub end_line: u32,
    pub chunk_type: crate::types::ChunkType,
    pub name: Option<String>,
    /// Hierarchical breadcrumb path from AST (e.g. "impl:AuthService > fn:login")
    pub context_path: Option<String>,
    pub project_id: String,
}

impl ChunkMeta {
    /// Convert a `CodeChunk` (from DB) into a `(ChunkMeta, content)` pair.
    ///
    /// The `content` string is returned separately so the caller can pass it to
    /// `rebuild_project` / `upsert_chunks` for BM25 document construction and
    /// then discard it — keeping `ChunkMeta` itself free of large strings.
    ///
    /// Returns `None` if the chunk has no DB-assigned ID.
    pub fn from_code_chunk(chunk: &crate::types::CodeChunk) -> Option<(Self, String)> {
        let id = chunk
            .id
            .as_ref()
            .map(|thing| crate::types::record_key_to_string(&thing.key))?;
        let project_id = chunk.project_id.clone().unwrap_or_default();
        let meta = Self {
            id,
            file_path: chunk.file_path.clone(),
            language: chunk.language.clone(),
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            chunk_type: chunk.chunk_type.clone(),
            name: chunk.name.clone(),
            context_path: chunk.context_path.clone(),
            project_id,
        };
        Some((meta, chunk.content.clone()))
    }
}

/// Thread-safe in-memory BM25 index sharded by project ID.
pub struct CodeSearchEngine {
    /// Per-project search engines, each protected by its own lock.
    /// The outer HashMap is wrapped in a RwLock so we can insert new projects.
    projects: RwLock<HashMap<String, ProjectIndex>>,
}

struct ProjectIndex {
    engine: InnerEngine,
    meta: HashMap<String, ChunkMeta>,
}

impl CodeSearchEngine {
    pub fn new() -> Self {
        Self {
            projects: RwLock::new(HashMap::new()),
        }
    }

    /// Replace the index for a project with the given set of chunks.
    /// This rebuilds the entire BM25 engine from scratch (safe for batch re-index).
    ///
    /// `chunks` is a `(ChunkMeta, content)` pair — `content` is used only to
    /// build the BM25 document text and is **not** stored in the index.
    ///
    /// The CPU-bound tokenization and index construction is offloaded to a
    /// `spawn_blocking` thread so the async executor is not blocked.  The
    /// `RwLock` is only acquired for the final `HashMap` insertion.
    pub async fn rebuild_project(&self, project_id: &str, chunks: Vec<(ChunkMeta, String)>) {
        self.rebuild_project_from_pairs(project_id, chunks).await
    }

    /// Internal: build a `ProjectIndex` from pre-assembled (meta, content) pairs
    /// and store it.  Uses `into_iter()` to consume `pairs` so each Document is
    /// built and the pair dropped, rather than keeping both alive simultaneously.
    async fn rebuild_project_from_pairs(&self, project_id: &str, pairs: Vec<(ChunkMeta, String)>) {
        // Move all CPU-bound work (document preparation + engine build) off the
        // async thread.
        let (engine, meta) = tokio::task::spawn_blocking(move || {
            // Consume pairs with into_iter() so each (ChunkMeta, String) is
            // dropped as soon as we extract what we need — avoids holding the
            // full content Vec alive while also building the documents Vec.
            let mut documents: Vec<Document<String>> = Vec::with_capacity(pairs.len());
            let mut meta_map: HashMap<String, ChunkMeta> = HashMap::with_capacity(pairs.len());

            for (chunk, content) in pairs {
                documents.push(Document::new(
                    chunk.id.clone(),
                    make_document_text(&chunk, &content),
                ));
                // `content` is dropped here — not stored in meta_map
                meta_map.insert(chunk.id.clone(), chunk);
            }

            let engine: InnerEngine = if documents.is_empty() {
                // Build an empty engine with a reasonable avgdl
                SearchEngineBuilder::<String, u32, CodeTokenizer>::with_avgdl(256.0).build()
            } else {
                SearchEngineBuilder::<String, u32, CodeTokenizer>::with_tokenizer_and_documents(
                    CodeTokenizer,
                    documents,
                )
                .build()
            };

            (engine, meta_map)
        })
        .await
        .expect("BM25 rebuild_project_from_pairs: spawn_blocking panicked");

        // Only hold the lock for the map insertion – no CPU work here.
        let mut projects = self.projects.write().await;
        projects.insert(project_id.to_string(), ProjectIndex { engine, meta });
    }

    /// Remove the index for a project (called on project deletion).
    pub async fn remove_project(&self, project_id: &str) {
        self.projects.write().await.remove(project_id);
    }

    /// Clear all in-memory BM25 project indexes.
    pub async fn clear_all(&self) {
        self.projects.write().await.clear();
    }

    /// Add or update individual chunks in the index (called after incremental indexing).
    ///
    /// `chunks` is a `(ChunkMeta, content)` pair — `content` is used only to
    /// build the BM25 document text and is **not** stored in the index.
    ///
    /// The existing `ProjectIndex` is temporarily removed from the map so it can
    /// be moved into a `spawn_blocking` closure for CPU-bound upsert work.  The
    /// updated index is then inserted back, and the `RwLock` is only held for
    /// the two map operations (remove + insert) — not during the CPU work.
    pub async fn upsert_chunks(&self, project_id: &str, chunks: Vec<(ChunkMeta, String)>) {
        // Take the existing index out of the map (avoid holding the lock during
        // CPU-bound work).
        let existing = {
            let mut projects = self.projects.write().await;
            projects.remove(project_id)
        };

        // Build a ProjectIndex (reuse existing or create a fresh one) and run
        // all tokenization / upsert work off the async thread.
        let updated = tokio::task::spawn_blocking(move || {
            let mut idx = existing.unwrap_or_else(|| ProjectIndex {
                engine: SearchEngineBuilder::<String, u32, CodeTokenizer>::with_avgdl(256.0)
                    .build(),
                meta: HashMap::new(),
            });

            for (chunk, content) in chunks {
                let doc = Document::new(chunk.id.clone(), make_document_text(&chunk, &content));
                idx.engine.upsert(doc);
                idx.meta.insert(chunk.id.clone(), chunk);
            }

            idx
        })
        .await
        .expect("BM25 upsert_chunks: spawn_blocking panicked");

        // Re-insert the updated index – only the lock for map insertion is held.
        let mut projects = self.projects.write().await;
        projects.insert(project_id.to_string(), updated);
    }

    /// Remove chunks for a specific file from the index.
    pub async fn remove_file_chunks(&self, project_id: &str, file_path: &str) {
        let mut projects = self.projects.write().await;
        if let Some(idx) = projects.get_mut(project_id) {
            let ids_to_remove: Vec<String> = idx
                .meta
                .values()
                .filter(|m| m.file_path == file_path)
                .map(|m| m.id.clone())
                .collect();
            for id in &ids_to_remove {
                idx.engine.remove(&id.to_string());
                idx.meta.remove(id);
            }
        }
    }

    /// Search across all projects or a specific project.
    ///
    /// After identifying the top-`limit` chunk IDs via the in-memory BM25 index,
    /// this method fetches the actual `content` for those chunks from `storage`.
    /// This avoids holding all chunk content in RAM (the OOM fix for Bug 2).
    pub async fn search(
        &self,
        query: &str,
        project_id: Option<&str>,
        limit: usize,
        storage: &impl StorageBackend,
    ) -> Vec<ScoredCodeChunk> {
        // ── Phase 1: rank chunks using in-memory BM25 (no content needed) ──
        let scored_metas: Vec<(f32, ChunkMeta)> = {
            let projects = self.projects.read().await;

            let mut all_results: Vec<(f32, ChunkMeta)> = Vec::new();

            let project_ids: Vec<&str> = match project_id {
                Some(pid) => vec![pid],
                None => projects.keys().map(|k| k.as_str()).collect(),
            };

            tracing::debug!(
                total_projects = projects.len(),
                requested_pid = ?project_id,
                available_pids = ?projects.keys().collect::<Vec<_>>(),
                "BM25 search: starting"
            );

            for pid in project_ids {
                if let Some(idx) = projects.get(pid) {
                    tracing::debug!(
                        project_id = pid,
                        meta_count = idx.meta.len(),
                        "BM25 search: querying project"
                    );
                    let results = idx.engine.search(query, limit * 2);
                    tracing::debug!(
                        project_id = pid,
                        raw_results = results.len(),
                        query = query,
                        "BM25 search: engine returned"
                    );
                    for result in results {
                        if let Some(meta) = idx.meta.get(&result.document.id) {
                            all_results.push((result.score, meta.clone()));
                        }
                    }
                } else {
                    tracing::warn!(project_id = pid, "BM25 search: project NOT FOUND in index");
                }
            }

            // Sort by score descending and take top `limit`
            all_results.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            all_results.truncate(limit);
            all_results
        };

        if scored_metas.is_empty() {
            return vec![];
        }

        // ── Phase 2: fetch content for only the top-N results from the DB ──
        let ids: Vec<String> = scored_metas.iter().map(|(_, m)| m.id.clone()).collect();
        let fetched = match storage.get_chunks_by_ids(&ids).await {
            Ok(chunks) => chunks,
            Err(e) => {
                tracing::warn!("BM25 search: failed to fetch chunk content: {}", e);
                vec![]
            }
        };

        // Build a content lookup by chunk ID
        let content_map: HashMap<String, String> = fetched
            .into_iter()
            .filter_map(|c| {
                c.id.as_ref()
                    .map(|t| crate::types::record_key_to_string(&t.key))
                    .map(|id| (id, c.content))
            })
            .collect();

        // ── Phase 3: construct ScoredCodeChunk results ──
        scored_metas
            .into_iter()
            .map(|(score, meta)| {
                let content = content_map.get(&meta.id).cloned().unwrap_or_default();
                ScoredCodeChunk {
                    id: meta.id,
                    file_path: meta.file_path,
                    content,
                    language: meta.language,
                    start_line: meta.start_line,
                    end_line: meta.end_line,
                    chunk_type: meta.chunk_type,
                    name: meta.name,
                    context_path: meta.context_path,
                    score,
                }
            })
            .collect()
    }

    /// Check if a project has a BM25 index loaded.
    pub async fn has_project(&self, project_id: &str) -> bool {
        self.projects.read().await.contains_key(project_id)
    }

    /// Return the number of chunks indexed for a project.
    pub async fn chunk_count(&self, project_id: &str) -> usize {
        self.projects
            .read()
            .await
            .get(project_id)
            .map(|idx| idx.meta.len())
            .unwrap_or(0)
    }

    /// Load all chunks for all indexed projects from storage into the BM25 index.
    /// Called once at startup to warm the in-memory index.
    ///
    /// Uses paginated fetching (1000 chunks per page) so at most one page of
    /// raw `CodeChunk` rows (~2 MB) is alive at any time instead of loading the
    /// entire project into RAM.
    pub async fn load_all_from_storage(
        &self,
        storage: &impl crate::storage::StorageBackend,
    ) -> usize {
        let projects = match storage.list_projects().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to list projects for BM25 warm-up: {}", e);
                return 0;
            }
        };

        let mut total = 0usize;

        for project_id in &projects {
            // Only load projects that finished indexing (completed or embedding_pending)
            match storage.get_index_status(project_id).await {
                Ok(Some(status))
                    if status.status == crate::types::IndexState::Completed
                        || status.status == crate::types::IndexState::EmbeddingPending =>
                {
                    // OK — load it
                }
                _ => continue,
            }

            match self.rebuild_project_streaming(storage, project_id).await {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(
                            project_id = %project_id,
                            chunks = count,
                            "BM25 index loaded for project"
                        );
                        total += count;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        project_id = %project_id,
                        error = %e,
                        "Failed to load chunks for BM25 index"
                    );
                }
            }
        }

        total
    }

    /// Stream-load chunks for a single project from storage page by page,
    /// building up the BM25 (meta, content) pairs without ever holding more
    /// than one page of raw `CodeChunk` rows in memory.
    ///
    /// Returns the number of chunks indexed.
    async fn rebuild_project_streaming(
        &self,
        storage: &impl crate::storage::StorageBackend,
        project_id: &str,
    ) -> crate::Result<usize> {
        const PAGE_SIZE: usize = 1000;
        let mut all_pairs: Vec<(ChunkMeta, String)> = Vec::new();
        let mut offset = 0usize;

        loop {
            let chunks = storage
                .get_chunks_paginated(project_id, PAGE_SIZE, offset)
                .await?;
            if chunks.is_empty() {
                break;
            }
            let page_len = chunks.len();
            // Consume `chunks` (not borrow) so the Vec<CodeChunk> is freed as
            // we iterate — only `all_pairs` accumulates.
            for chunk in chunks {
                if let Some(pair) = ChunkMeta::from_code_chunk(&chunk) {
                    all_pairs.push(pair);
                }
            }
            offset += page_len;
            if page_len < PAGE_SIZE {
                break;
            }
        }

        let count = all_pairs.len();
        if count > 0 {
            self.rebuild_project_from_pairs(project_id, all_pairs).await;
        }
        Ok(count)
    }

    /// Rebuild the BM25 index for a single project from storage.
    /// Called after indexing completes.
    ///
    /// Uses the same streaming paginated fetch as `load_all_from_storage` to
    /// avoid a transient memory spike.
    pub async fn rebuild_from_storage(
        &self,
        storage: &impl crate::storage::StorageBackend,
        project_id: &str,
    ) {
        match self.rebuild_project_streaming(storage, project_id).await {
            Ok(count) => {
                tracing::info!(
                    project_id = %project_id,
                    chunks = count,
                    "BM25 index rebuilt for project"
                );
            }
            Err(e) => {
                tracing::warn!(
                    project_id = %project_id,
                    error = %e,
                    "Failed to rebuild BM25 index for project"
                );
            }
        }
    }
}

impl Default for CodeSearchEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the text that goes into the BM25 document for a chunk.
/// We include file_path + name + content so symbol names are findable.
///
/// For each structured field (file_path, name) we emit BOTH:
///   1. The raw unsplit value (e.g. `storage/surrealdb.rs`) so the tokenizer
///      can see the whole token and produce an exact-match signal for queries
///      like `surrealdb.rs` or `storage/surrealdb`.
///   2. The space-normalised version (e.g. `storage surrealdb rs`) so
///      individual path components are also individually searchable.
///
/// `content` is passed separately rather than read from `ChunkMeta` because
/// the `content` field was removed from `ChunkMeta` to avoid OOM.
fn make_document_text(chunk: &ChunkMeta, content: &str) -> String {
    let mut text = String::with_capacity(content.len() + chunk.file_path.len() * 2 + 128);

    // ── file_path ──────────────────────────────────────────────────────────
    // Raw form: tokenizer will see "storage/surrealdb.rs" as one token and
    // also split it on '/' and '.' into sub-tokens.
    text.push_str(&chunk.file_path);
    text.push(' ');
    // Normalised form: replaces path separators / name separators with spaces
    // so each component ("storage", "surrealdb", "rs") is a separate token.
    text.push_str(&chunk.file_path.replace(['/', '\\', '.', ':'], " "));
    text.push(' ');

    // ── symbol name ────────────────────────────────────────────────────────
    if let Some(ref name) = chunk.name {
        // Raw form (e.g. "MyModule.some_fn" or "MyModule::some_fn")
        text.push_str(name);
        text.push(' ');
        // Normalised form
        text.push_str(&name.replace(['.', ':', '/'], " "));
        text.push(' ');
    }

    // ── context_path (breadcrumbs) ─────────────────────────────────────────
    // Adds hierarchical scope info so queries like "AuthService login" match
    // a chunk with context_path "impl:AuthService > fn:login" even if the
    // chunk content itself only contains the function body.
    if let Some(ref ctx) = chunk.context_path {
        // Raw form preserves the structured breadcrumb for exact-match
        text.push_str(ctx);
        text.push(' ');
        // Normalised form: replace delimiters with spaces so each scope
        // component ("impl", "AuthService", "fn", "login") is a separate token
        text.push_str(&ctx.replace(['>', ':', '.', '/'], " "));
        text.push(' ');
    }

    // ── code content ───────────────────────────────────────────────────────
    text.push_str(content);
    text
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(input: &str) -> Vec<String> {
        let tok = CodeTokenizer;
        tok.tokenize(input).collect()
    }

    // ── Minimal mock storage for unit tests ──────────────────────────────────
    //
    // Only `get_chunks_by_ids` is needed by `search()`.  All other methods are
    // unreachable in unit-test context and will panic if accidentally called.

    struct MockStorage {
        /// Chunks returned by `get_chunks_by_ids`, keyed by string ID.
        chunks: std::collections::HashMap<String, crate::types::CodeChunk>,
    }

    impl MockStorage {
        fn new(chunks: Vec<crate::types::CodeChunk>) -> Self {
            let map = chunks
                .into_iter()
                .filter_map(|c| {
                    c.id.as_ref()
                        .map(|t| crate::types::record_key_to_string(&t.key))
                        .map(|id| (id, c.clone()))
                        .or_else(|| {
                            // Chunks without an ID (no Thing) can be keyed by
                            // an empty string – they won't be found during look-up
                            // so it's safe to skip them entirely.
                            None
                        })
                })
                .collect();
            Self { chunks: map }
        }

        /// Build a chunk suitable for look-up by a plain string ID (no table prefix).
        fn make_chunk(id: &str, content: &str) -> crate::types::CodeChunk {
            use crate::types::RecordId;
            crate::types::CodeChunk {
                id: Some(RecordId::new("code_chunks", id)),
                file_path: format!("src/{}.rs", id),
                content: content.to_string(),
                language: crate::types::Language::Rust,
                start_line: 1,
                end_line: 3,
                chunk_type: crate::types::ChunkType::Function,
                name: None,
                context_path: None,
                embedding: None,
                content_hash: String::new(),
                project_id: Some("proj".to_string()),
                indexed_at: crate::types::Datetime::default(),
            }
        }
    }

    impl crate::storage::StorageBackend for MockStorage {
        async fn get_chunks_by_ids(
            &self,
            ids: &[String],
        ) -> crate::Result<Vec<crate::types::CodeChunk>> {
            Ok(ids
                .iter()
                .filter_map(|id| self.chunks.get(id).cloned())
                .collect())
        }

        // ── All other methods are unreachable in unit tests ──────────────────
        async fn create_memory(&self, _: crate::types::Memory) -> crate::Result<String> {
            unreachable!()
        }
        async fn get_memory(&self, _: &str) -> crate::Result<Option<crate::types::Memory>> {
            unreachable!()
        }
        async fn update_memory(
            &self,
            _: &str,
            _: crate::types::MemoryUpdate,
        ) -> crate::Result<crate::types::Memory> {
            unreachable!()
        }
        async fn delete_memory(&self, _: &str) -> crate::Result<bool> {
            unreachable!()
        }
        async fn list_memories(
            &self,
            _: usize,
            _: usize,
        ) -> crate::Result<Vec<crate::types::Memory>> {
            unreachable!()
        }
        async fn count_memories(&self) -> crate::Result<usize> {
            unreachable!()
        }
        async fn vector_search(
            &self,
            _: &[f32],
            _: usize,
        ) -> crate::Result<Vec<crate::types::SearchResult>> {
            unreachable!()
        }
        async fn vector_search_code(
            &self,
            _: &[f32],
            _: Option<&str>,
            _: usize,
        ) -> crate::Result<Vec<crate::types::ScoredCodeChunk>> {
            unreachable!()
        }
        async fn vector_search_symbols(
            &self,
            _: &[f32],
            _: Option<&str>,
            _: usize,
        ) -> crate::Result<Vec<crate::types::CodeSymbol>> {
            unreachable!()
        }
        async fn bm25_search(
            &self,
            _: &str,
            _: usize,
        ) -> crate::Result<Vec<crate::types::SearchResult>> {
            unreachable!()
        }
        async fn bm25_search_code(
            &self,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> crate::Result<Vec<crate::types::ScoredCodeChunk>> {
            unreachable!()
        }
        async fn clear_project_embeddings(&self, _: &str) -> crate::Result<u64> {
            unreachable!()
        }
        async fn create_entity(&self, _: crate::types::Entity) -> crate::Result<String> {
            unreachable!()
        }
        async fn get_entity(&self, _: &str) -> crate::Result<Option<crate::types::Entity>> {
            unreachable!()
        }
        async fn search_entities(
            &self,
            _: &str,
            _: usize,
        ) -> crate::Result<Vec<crate::types::Entity>> {
            unreachable!()
        }
        async fn create_relation(&self, _: crate::types::Relation) -> crate::Result<String> {
            unreachable!()
        }
        async fn get_related(
            &self,
            _: &str,
            _: usize,
            _: crate::types::Direction,
        ) -> crate::Result<(Vec<crate::types::Entity>, Vec<crate::types::Relation>)> {
            unreachable!()
        }
        async fn get_subgraph(
            &self,
            _: &[String],
        ) -> crate::Result<(Vec<crate::types::Entity>, Vec<crate::types::Relation>)> {
            unreachable!()
        }
        async fn get_node_degrees(
            &self,
            _: &[String],
        ) -> crate::Result<std::collections::HashMap<String, usize>> {
            unreachable!()
        }
        async fn get_all_entities(&self) -> crate::Result<Vec<crate::types::Entity>> {
            unreachable!()
        }
        async fn get_all_relations(&self) -> crate::Result<Vec<crate::types::Relation>> {
            unreachable!()
        }
        async fn get_valid(
            &self,
            _: Option<&str>,
            _: usize,
        ) -> crate::Result<Vec<crate::types::Memory>> {
            unreachable!()
        }
        async fn get_valid_at(
            &self,
            _: crate::types::Datetime,
            _: Option<&str>,
            _: usize,
        ) -> crate::Result<Vec<crate::types::Memory>> {
            unreachable!()
        }
        async fn invalidate(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> crate::Result<bool> {
            unreachable!()
        }
        async fn create_code_chunk(&self, _: crate::types::CodeChunk) -> crate::Result<String> {
            unreachable!()
        }
        async fn create_code_chunks_batch(
            &self,
            _: Vec<crate::types::CodeChunk>,
        ) -> crate::Result<Vec<(String, crate::types::CodeChunk)>> {
            unreachable!()
        }
        async fn delete_project_chunks(&self, _: &str) -> crate::Result<usize> {
            unreachable!()
        }
        async fn delete_chunks_by_path(&self, _: &str, _: &str) -> crate::Result<usize> {
            unreachable!()
        }
        async fn get_chunks_by_path(
            &self,
            _: &str,
            _: &str,
        ) -> crate::Result<Vec<crate::types::CodeChunk>> {
            unreachable!()
        }
        async fn get_all_chunks_for_project(
            &self,
            _: &str,
        ) -> crate::Result<Vec<crate::types::CodeChunk>> {
            unreachable!()
        }
        async fn get_chunks_paginated(
            &self,
            _: &str,
            _: usize,
            _: usize,
        ) -> crate::Result<Vec<crate::types::CodeChunk>> {
            unreachable!()
        }
        async fn get_index_status(
            &self,
            _: &str,
        ) -> crate::Result<Option<crate::types::IndexStatus>> {
            unreachable!()
        }
        async fn update_index_status(&self, _: crate::types::IndexStatus) -> crate::Result<()> {
            unreachable!()
        }
        async fn delete_index_status(&self, _: &str) -> crate::Result<()> {
            unreachable!()
        }
        async fn list_projects(&self) -> crate::Result<Vec<String>> {
            unreachable!()
        }
        async fn get_file_hash(&self, _: &str, _: &str) -> crate::Result<Option<String>> {
            unreachable!()
        }
        async fn set_file_hash(&self, _: &str, _: &str, _: &str) -> crate::Result<()> {
            unreachable!()
        }
        async fn set_file_hashes_batch(
            &self,
            _: &str,
            _: &[(String, String)],
        ) -> crate::Result<()> {
            unreachable!()
        }
        async fn delete_file_hashes(&self, _: &str) -> crate::Result<()> {
            unreachable!()
        }
        async fn delete_file_hash(&self, _: &str, _: &str) -> crate::Result<()> {
            unreachable!()
        }
        async fn create_code_symbol(&self, _: crate::types::CodeSymbol) -> crate::Result<String> {
            unreachable!()
        }
        async fn create_code_symbols_batch(
            &self,
            _: Vec<crate::types::CodeSymbol>,
        ) -> crate::Result<Vec<String>> {
            unreachable!()
        }
        async fn update_symbol_embedding(&self, _: &str, _: Vec<f32>) -> crate::Result<()> {
            unreachable!()
        }
        async fn update_chunk_embedding(&self, _: &str, _: Vec<f32>) -> crate::Result<()> {
            unreachable!()
        }
        async fn batch_update_symbol_embeddings(
            &self,
            _: &[(String, Vec<f32>)],
        ) -> crate::Result<()> {
            unreachable!()
        }
        async fn batch_update_chunk_embeddings(
            &self,
            _: &[(String, Vec<f32>)],
        ) -> crate::Result<()> {
            unreachable!()
        }
        async fn create_symbol_relation(
            &self,
            _: crate::types::SymbolRelation,
        ) -> crate::Result<String> {
            unreachable!()
        }
        async fn create_symbol_relations_batch(
            &self,
            _: Vec<crate::types::SymbolRelation>,
        ) -> crate::Result<u32> {
            unreachable!()
        }
        async fn delete_project_symbols(&self, _: &str) -> crate::Result<usize> {
            unreachable!()
        }
        async fn delete_symbols_by_path(&self, _: &str, _: &str) -> crate::Result<usize> {
            unreachable!()
        }
        async fn get_project_symbols(
            &self,
            _: &str,
        ) -> crate::Result<Vec<crate::types::CodeSymbol>> {
            unreachable!()
        }
        async fn get_symbol_callers(
            &self,
            _: &str,
        ) -> crate::Result<Vec<crate::types::CodeSymbol>> {
            unreachable!()
        }
        async fn get_symbol_callees(
            &self,
            _: &str,
        ) -> crate::Result<Vec<crate::types::CodeSymbol>> {
            unreachable!()
        }
        async fn get_related_symbols(
            &self,
            _: &str,
            _: usize,
            _: crate::types::Direction,
        ) -> crate::Result<(
            Vec<crate::types::CodeSymbol>,
            Vec<crate::types::SymbolRelation>,
        )> {
            unreachable!()
        }
        async fn get_code_subgraph(
            &self,
            _: &[String],
        ) -> crate::Result<(
            Vec<crate::types::CodeSymbol>,
            Vec<crate::types::SymbolRelation>,
        )> {
            unreachable!()
        }
        async fn search_symbols(
            &self,
            _: &str,
            _: Option<&str>,
            _: usize,
            _: usize,
            _: Option<&str>,
            _: Option<&str>,
        ) -> crate::Result<(Vec<crate::types::CodeSymbol>, u32)> {
            unreachable!()
        }
        async fn replace_symbol_chunk_map(
            &self,
            _: &str,
            _: &[(String, String, f32)],
        ) -> crate::Result<u32> {
            unreachable!()
        }
        async fn get_mapped_chunks_for_symbols(
            &self,
            _: &str,
            _: &[String],
            _: usize,
        ) -> crate::Result<Vec<(String, f32)>> {
            unreachable!()
        }
        async fn count_symbols(&self, _: &str) -> crate::Result<u32> {
            unreachable!()
        }
        async fn count_chunks(&self, _: &str) -> crate::Result<u32> {
            unreachable!()
        }
        async fn count_embedded_symbols(&self, _: &str) -> crate::Result<u32> {
            unreachable!()
        }
        async fn count_embedded_chunks(&self, _: &str) -> crate::Result<u32> {
            unreachable!()
        }
        async fn get_unembedded_chunks(
            &self,
            _project_id: &str,
        ) -> crate::Result<Vec<(String, String)>> {
            Ok(vec![])
        }
        async fn get_unembedded_symbols(
            &self,
            _project_id: &str,
        ) -> crate::Result<Vec<(String, String)>> {
            Ok(vec![])
        }
        async fn count_symbol_relations(&self, _: &str) -> crate::Result<u32> {
            unreachable!()
        }
        async fn find_symbol_by_name(
            &self,
            _: &str,
            _: &str,
        ) -> crate::Result<Option<crate::types::CodeSymbol>> {
            unreachable!()
        }
        async fn find_symbols_by_names(
            &self,
            _: &str,
            _: &[String],
        ) -> crate::Result<Vec<crate::types::CodeSymbol>> {
            unreachable!()
        }
        async fn find_symbol_by_name_with_context(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> crate::Result<Option<crate::types::CodeSymbol>> {
            unreachable!()
        }
        async fn health_check(&self) -> crate::Result<bool> {
            unreachable!()
        }
        async fn reset_db(&self) -> crate::Result<()> {
            unreachable!()
        }
        async fn shutdown(&self) -> crate::Result<()> {
            unreachable!()
        }
        async fn upsert_manifest_entry(&self, _: &str, _: &str) -> crate::Result<()> {
            unreachable!()
        }
        async fn upsert_manifest_entries(&self, _: &str, _: &[String]) -> crate::Result<()> {
            unreachable!()
        }
        async fn get_manifest_entries(
            &self,
            _: &str,
        ) -> crate::Result<Vec<crate::types::ManifestEntry>> {
            unreachable!()
        }
        async fn delete_manifest_entries(&self, _: &str) -> crate::Result<()> {
            unreachable!()
        }
        async fn delete_manifest_entry(&self, _: &str, _: &str) -> crate::Result<()> {
            unreachable!()
        }
        async fn count_manifest_entries(&self, _: &str) -> crate::Result<usize> {
            unreachable!()
        }
    }

    // ── Helper to build (ChunkMeta, content) pairs for test index construction ──

    fn make_chunk_pair(
        id: &str,
        file_path: &str,
        content: &str,
        name: Option<&str>,
        project_id: &str,
    ) -> (ChunkMeta, String) {
        (
            ChunkMeta {
                id: id.to_string(),
                file_path: file_path.to_string(),
                language: crate::types::Language::Rust,
                start_line: 1,
                end_line: 3,
                chunk_type: crate::types::ChunkType::Function,
                name: name.map(|s| s.to_string()),
                context_path: None,
                project_id: project_id.to_string(),
            },
            content.to_string(),
        )
    }

    #[test]
    fn test_snake_case() {
        let t = tokens("snake_case_variable");
        assert!(t.contains(&"snake".to_string()), "{t:?}");
        assert!(t.contains(&"case".to_string()), "{t:?}");
        assert!(t.contains(&"variable".to_string()), "{t:?}");
    }

    #[test]
    fn test_camel_case() {
        let t = tokens("camelCaseFunction");
        assert!(t.contains(&"camel".to_string()), "{t:?}");
        assert!(t.contains(&"case".to_string()), "{t:?}");
        assert!(t.contains(&"function".to_string()), "{t:?}");
        // full token is also present
        assert!(t.contains(&"camelcasefunction".to_string()), "{t:?}");
    }

    #[test]
    fn test_pascal_case() {
        let t = tokens("OdooAuthService");
        assert!(t.contains(&"odoo".to_string()), "{t:?}");
        assert!(t.contains(&"auth".to_string()), "{t:?}");
        assert!(t.contains(&"service".to_string()), "{t:?}");
        // full lowercase token for exact match
        assert!(t.contains(&"odooauthservice".to_string()), "{t:?}");
    }

    #[test]
    fn test_http_acronym() {
        let t = tokens("HTTPRequestType");
        assert!(t.contains(&"http".to_string()), "{t:?}");
        assert!(t.contains(&"request".to_string()), "{t:?}");
        assert!(t.contains(&"type".to_string()), "{t:?}");
    }

    #[test]
    fn test_digit_transition() {
        let t = tokens("HTML5Parser");
        // "html5" stays together (upper sequence + digit)
        assert!(t.contains(&"parser".to_string()), "{t:?}");
    }

    #[test]
    fn test_kebab_case() {
        let t = tokens("kebab-case-id");
        assert!(t.contains(&"kebab".to_string()), "{t:?}");
        assert!(t.contains(&"case".to_string()), "{t:?}");
        assert!(t.contains(&"id".to_string()), "{t:?}");
    }

    #[tokio::test]
    async fn test_search_engine_finds_symbol() {
        let engine = CodeSearchEngine::new();

        let chunks = vec![
            make_chunk_pair(
                "chunk1",
                "src/auth/service.rs",
                "fn authenticate_user(token: &str) -> bool { ... }",
                Some("authenticate_user"),
                "proj",
            ),
            make_chunk_pair(
                "chunk2",
                "src/db/query.rs",
                "fn fetch_users() -> Vec<User> { ... }",
                Some("fetch_users"),
                "proj",
            ),
        ];

        // Prepare MockStorage so search() can hydrate content
        let mock_storage = MockStorage::new(vec![
            MockStorage::make_chunk(
                "chunk1",
                "fn authenticate_user(token: &str) -> bool { ... }",
            ),
            MockStorage::make_chunk("chunk2", "fn fetch_users() -> Vec<User> { ... }"),
        ]);

        engine.rebuild_project("proj", chunks).await;

        let results = engine
            .search("authenticate", Some("proj"), 5, &mock_storage)
            .await;
        assert!(!results.is_empty(), "Should find authenticate_user chunk");
        assert_eq!(results[0].id, "chunk1");
        assert!(results[0].score > 0.0);
    }

    /// Regression test for Bug 1: raw path/dotted-name scoring.
    ///
    /// Before the fix `make_document_text` replaced `/` and `.` with spaces
    /// before tokenisation, so a query for `surrealdb.rs` or
    /// `storage/surrealdb` would not receive the exact-match IDF boost.
    /// After the fix both the raw and normalised forms are emitted.
    #[tokio::test]
    async fn test_exact_path_scoring() {
        let engine = CodeSearchEngine::new();

        let chunks = vec![
            make_chunk_pair(
                "c_surreal",
                "storage/surrealdb.rs",
                "pub struct SurrealStorage {}",
                Some("SurrealStorage"),
                "proj",
            ),
            make_chunk_pair(
                "c_memory",
                "storage/memory.rs",
                "pub struct MemoryStorage {}",
                Some("MemoryStorage"),
                "proj",
            ),
        ];

        let mock_storage = MockStorage::new(vec![
            MockStorage::make_chunk("c_surreal", "pub struct SurrealStorage {}"),
            MockStorage::make_chunk("c_memory", "pub struct MemoryStorage {}"),
        ]);

        engine.rebuild_project("proj", chunks).await;

        // Query by exact filename — should rank surrealdb.rs chunk first
        let results = engine
            .search("surrealdb.rs", Some("proj"), 5, &mock_storage)
            .await;
        assert!(!results.is_empty(), "Should find surrealdb.rs chunk");
        assert_eq!(
            results[0].id,
            "c_surreal",
            "surrealdb.rs chunk must rank first; got {:?}",
            results.iter().map(|r| (&r.id, r.score)).collect::<Vec<_>>()
        );

        // Query by dotted name component — raw token "surrealdb" must score higher
        // than "memory" for the memory.rs chunk
        let results2 = engine
            .search("surrealdb", Some("proj"), 5, &mock_storage)
            .await;
        assert!(!results2.is_empty(), "Should find surrealdb chunk");
        assert_eq!(
            results2[0].id, "c_surreal",
            "surrealdb query must rank SurrealStorage first"
        );

        // Query by full path segment including slash
        let results3 = engine
            .search("storage/surrealdb", Some("proj"), 5, &mock_storage)
            .await;
        assert!(!results3.is_empty(), "Should find storage/surrealdb chunk");
        assert_eq!(
            results3[0].id, "c_surreal",
            "path query must rank SurrealStorage first"
        );
    }

    /// Verify make_document_text emits both raw and normalised forms.
    #[test]
    fn test_make_document_text_includes_raw_path() {
        let chunk = ChunkMeta {
            id: "x".to_string(),
            file_path: "storage/surrealdb.rs".to_string(),
            language: crate::types::Language::Rust,
            start_line: 1,
            end_line: 1,
            chunk_type: crate::types::ChunkType::Other,
            name: Some("MyModule::my_fn".to_string()),
            context_path: None,
            project_id: "p".to_string(),
        };
        let content = "some content here";
        let text = make_document_text(&chunk, content);
        // Raw path must appear verbatim
        assert!(
            text.contains("storage/surrealdb.rs"),
            "raw file_path must be present: {text:?}"
        );
        // Normalised form must also appear
        assert!(
            text.contains("storage surrealdb rs"),
            "normalised path must be present: {text:?}"
        );
        // Raw name
        assert!(
            text.contains("MyModule::my_fn"),
            "raw name must be present: {text:?}"
        );
        // Content
        assert!(
            text.contains("some content here"),
            "content must be present: {text:?}"
        );
    }
}
