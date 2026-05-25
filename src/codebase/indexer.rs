use std::path::Path;
use std::sync::Arc;

use tokio::fs;

use crate::config::AppState;
use crate::storage::StorageBackend;
use crate::types::{IndexState, IndexStatus};
use crate::Result;

use super::chunker::chunk_file;
use super::parser::CodeParser;
use super::relations::{create_symbol_relations, detect_containment_references, RelationStats};
use super::scanner::scan_directory;
use super::symbol_index::SymbolIndex;

use crate::embedding::{EmbeddingRequest, EmbeddingTarget};
use crate::types::code::CodeChunk;
use crate::types::symbol::{CodeReference, CodeSymbol};

const MAX_CHUNKS_PER_FILE: usize = 50;
const MAX_CONCURRENT_PARSES: usize = 4;

pub async fn index_project(state: Arc<AppState>, project_path: &Path) -> Result<IndexStatus> {
    let project_id = project_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let state_clone = state.clone();
    let project_path_clone = project_path.to_path_buf();
    let project_id_clone = project_id.clone();

    // Spawn as a task so we can catch panics natively
    let handle = tokio::spawn(async move {
        do_index_project(state_clone, &project_path_clone, &project_id_clone).await
    });

    let result = match handle.await {
        Ok(res) => res,
        Err(join_err) => {
            let msg = if join_err.is_panic() {
                "Indexing panicked (crashed)".to_string()
            } else if join_err.is_cancelled() {
                "Indexing was cancelled".to_string()
            } else {
                "Indexing failed to complete".to_string()
            };
            Err(crate::AppError::Internal(msg.into()))
        }
    };

    match result {
        Ok(status) => Ok(status),
        Err(e) => {
            tracing::error!(project_id = %project_id, error = %e, "Indexing failed");
            let mut status = IndexStatus::new(project_id.clone());
            if let Ok(Some(existing)) = state.storage.get_index_status(&project_id).await {
                status = existing;
            }

            // Extract last known file if possible
            if let Some(monitor) = state.progress.get(&project_id).await {
                if let Ok(cf) = monitor.current_file.read() {
                    if !cf.is_empty() {
                        status.failed_files.push(cf.clone());
                        status.error_message = Some(format!("{}: Failed at file {}", e, cf));
                    } else {
                        status.error_message = Some(e.to_string());
                    }
                } else {
                    status.error_message = Some(e.to_string());
                }
            } else {
                status.error_message = Some(e.to_string());
            }

            status.status = IndexState::Failed;
            status.completed_at = Some(crate::types::Datetime::default());
            let _ = state.storage.update_index_status(status.clone()).await;
            Err(e)
        }
    }
}

async fn do_index_project(
    state: Arc<AppState>,
    project_path: &Path,
    project_id: &str,
) -> Result<IndexStatus> {
    let mut status = IndexStatus::new(project_id.to_string());
    let monitor = state.progress.get_or_create(project_id).await;

    state.storage.delete_project_chunks(project_id).await?;
    state.storage.delete_project_symbols(project_id).await?;
    state.storage.delete_file_hashes(project_id).await?;

    // `scan_directory` uses `ignore::WalkBuilder` — a synchronous blocking I/O walk.
    // Wrap in `spawn_blocking` to avoid starving the Tokio async thread pool.
    let project_path_for_scan = project_path.to_path_buf();
    let files = tokio::task::spawn_blocking(move || scan_directory(&project_path_for_scan))
        .await
        .map_err(|e| crate::AppError::Internal(format!("scan_directory panicked: {e}").into()))??;
    status.total_files = files.len() as u32;
    tracing::info!(
        project = %project_id,
        total_files = status.total_files,
        "Indexing started"
    );
    monitor
        .total_files
        .store(status.total_files, std::sync::atomic::Ordering::Relaxed);
    monitor
        .indexed_files
        .store(0, std::sync::atomic::Ordering::Relaxed);

    state.storage.update_index_status(status.clone()).await?;

    let batch_size = 100;
    let mut chunk_buffer = Vec::with_capacity(batch_size);
    let mut symbol_buffer = Vec::with_capacity(batch_size);
    let mut symbol_index = SymbolIndex::new();
    let mut relation_buffer: Vec<CodeReference> = Vec::new();
    let mut total_relation_stats = RelationStats::default();
    // Buffer file hashes for batched UPSERT (Bug 3 fix: avoids N sequential DB round-trips)
    let mut hash_buffer: Vec<(String, String)> = Vec::with_capacity(batch_size);
    const HASH_FLUSH_SIZE: usize = 50;

    // Issue 4 fix: Parse files with bounded concurrency using JoinSet instead of
    // sequential spawn_blocking. Up to max_concurrent_parses files are parsed on
    // the blocking thread pool simultaneously.
    #[allow(clippy::type_complexity)]
    let max_concurrent_parses = MAX_CONCURRENT_PARSES;
    #[allow(clippy::type_complexity)]
    let mut parse_set: tokio::task::JoinSet<(
        Vec<CodeChunk>,
        Vec<CodeSymbol>,
        Vec<CodeReference>,
        String,
    )> = tokio::task::JoinSet::new();

    // Macro to process one completed parse result (used in drain-when-full and final drain).
    // Expands in place so it can mutate surrounding locals and use `.await`.
    macro_rules! drain_one_parse {
        ($join_result:expr) => {{
            let (mut chunks, symbols, references, fp_str) = $join_result
                .map_err(|e| crate::AppError::Internal(
                    format!("parse/chunk panicked: {e}").into(),
                ))?;

            if chunks.len() > MAX_CHUNKS_PER_FILE {
                tracing::warn!(
                    file = %fp_str,
                    total_chunks = chunks.len(),
                    kept_chunks = MAX_CHUNKS_PER_FILE,
                    "Too many chunks in file, truncating",
                );
                chunks.truncate(MAX_CHUNKS_PER_FILE);
            }

            for chunk in chunks {
                chunk_buffer.push(chunk);
                status.total_chunks += 1;

                if chunk_buffer.len() >= batch_size {
                    let batch = std::mem::take(&mut chunk_buffer);
                    let _permit = state.db_semaphore.acquire().await;
                    if let Ok(results) = state.storage.create_code_chunks_batch(batch).await {
                        for (id, chunk) in results {
                            if let Err(e) = state
                                .embedding_queue
                                .send(EmbeddingRequest {
                                    text: chunk.content,
                                    responder: None,
                                    target: Some(EmbeddingTarget::Chunk(id.clone())),
                                    retry_count: 0,
                                })
                                .await
                            {
                                tracing::warn!(chunk_id = %id, error = %e, "Failed to enqueue chunk embedding");
                            }
                        }
                    }
                }
            }

            if !symbols.is_empty() {
                tracing::debug!(file = %fp_str, count = symbols.len(), "Parsed symbols");
            }

            // Add symbols to in-memory index (for cross-file relation resolution at end)
            for symbol in &symbols {
                symbol_index.add(symbol);
            }

            // Detect containment (parent→child) relations from symbol line ranges
            // BEFORE consuming `symbols` into the buffer. This produces Contains
            // edges (e.g. impl→fn, class→method) that tree-sitter doesn't emit.
            let containment_refs = detect_containment_references(&symbols);

            for symbol in symbols {
                symbol_buffer.push(symbol);
                status.total_symbols += 1;

                if symbol_buffer.len() >= batch_size {
                    let batch = std::mem::take(&mut symbol_buffer);
                    let _permit = state.db_semaphore.acquire().await;
                    match state.storage.create_code_symbols_batch(batch.clone()).await {
                        Ok(ids) => {
                            for (id, sym) in ids.iter().zip(batch.iter()) {
                                let embed_text = sym
                                    .signature
                                    .clone()
                                    .unwrap_or_else(|| format!("{} {}", sym.symbol_type, sym.name));
                                if let Err(e) = state
                                    .embedding_queue
                                    .send(EmbeddingRequest {
                                        text: embed_text,
                                        responder: None,
                                        target: Some(EmbeddingTarget::Symbol(id.clone())),
                                        retry_count: 0,
                                    })
                                    .await
                                {
                                    tracing::warn!(symbol_id = %id, error = %e, "Failed to enqueue symbol embedding");
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                count = batch.len(),
                                error = %e,
                                "Failed to store symbol batch"
                            );
                        }
                    }
                }
            }

            // Buffer references for deferred processing (after ALL symbols are indexed)
            relation_buffer.extend(references);
            relation_buffer.extend(containment_refs);

            // Keep references buffered until all symbols are indexed.
            // This preserves forward references to symbols defined later
            // in the scan order.

            status.indexed_files += 1;
            monitor
                .indexed_files
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            if status.indexed_files % 10 == 0 {
                let percent =
                    (status.indexed_files as f32 / status.total_files as f32 * 100.0) as u32;
                tracing::info!(
                    indexed = status.indexed_files,
                    total = status.total_files,
                    percent,
                    chunks = status.total_chunks,
                    symbols = status.total_symbols,
                    failed = status.failed_files.len(),
                    "Indexing progress"
                );
                if let Err(e) = state.storage.update_index_status(status.clone()).await {
                    tracing::warn!("Failed to update intermediate status: {}", e);
                }
            }
        }};
    }

    for file_path in &files {
        // Update current file in monitor for status reporting
        if let Ok(mut cf) = monitor.current_file.write() {
            *cf = file_path.to_string_lossy().to_string();
        }

        tracing::info!("Indexing file: {:?}", file_path);

        // Skip auto-generated files (no useful semantic content)
        if crate::codebase::scanner::is_ignored_file(file_path) {
            tracing::debug!(path = ?file_path, "Skipping generated file");
            status.indexed_files += 1;
            continue;
        }

        // Warn on large files but still process them (with chunk cap)
        if let Ok(meta) = fs::metadata(file_path).await {
            if meta.len() > 1_048_576 {
                tracing::warn!(
                    path = ?file_path,
                    size_kb = meta.len() / 1024,
                    "Large file detected (>1MB), might take a while to parse",

                );
            }
        }

        let content = match fs::read_to_string(file_path).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read file {:?}: {}", file_path, e);
                status
                    .failed_files
                    .push(file_path.to_string_lossy().to_string());
                continue;
            }
        };

        // Skip massive files to prevent OOM/TreeSitter crashes (e.g. giant bundled JS or Dart files)
        if content.len() > 1_000_000 {
            // > 1MB
            tracing::warn!("Skipping large file (>1MB): {:?}", file_path);
            status
                .failed_files
                .push(file_path.to_string_lossy().to_string());
            continue;
        }

        // Compute file-level hash for incremental indexing and buffer for batch flush
        let file_path_str = file_path.to_string_lossy().to_string();
        let file_hash = blake3::hash(content.as_bytes()).to_hex().to_string();
        hash_buffer.push((file_path_str, file_hash));

        // Flush hash buffer periodically to avoid unbounded growth
        if hash_buffer.len() >= HASH_FLUSH_SIZE {
            let batch = std::mem::take(&mut hash_buffer);
            let _ = state
                .storage
                .set_file_hashes_batch(project_id, &batch)
                .await;
        }

        // Issue 4 fix: Bounded-concurrency parsing via JoinSet.
        // Drain one completed parse result before spawning if at capacity.
        if parse_set.len() >= max_concurrent_parses {
            if let Some(join_result) = parse_set.join_next().await {
                drain_one_parse!(join_result);
            }
        }

        // Spawn CPU-bound chunk+parse work onto the blocking thread pool.
        // `content` is moved (not cloned) — the hash was already computed above.
        let file_path_for_blocking = file_path.clone();
        let project_id_for_blocking = project_id.to_string();
        parse_set.spawn_blocking(move || {
            let fp_str = file_path_for_blocking.to_string_lossy().to_string();
            let chunks = chunk_file(&file_path_for_blocking, &content, &project_id_for_blocking);
            let (symbols, references) =
                CodeParser::parse_file(&file_path_for_blocking, &content, &project_id_for_blocking);
            (chunks, symbols, references, fp_str)
        });
    }

    // Drain remaining in-flight parse tasks from the JoinSet
    while let Some(join_result) = parse_set.join_next().await {
        drain_one_parse!(join_result);
    }

    // Flush any remaining buffered file hashes
    if !hash_buffer.is_empty() {
        let _ = state
            .storage
            .set_file_hashes_batch(project_id, &hash_buffer)
            .await;
    }

    if !chunk_buffer.is_empty() {
        let _permit = state.db_semaphore.acquire().await;
        if let Ok(results) = state.storage.create_code_chunks_batch(chunk_buffer).await {
            for (id, chunk) in results {
                let _ = state
                    .embedding_queue
                    .send(EmbeddingRequest {
                        text: chunk.content,
                        responder: None,
                        target: Some(EmbeddingTarget::Chunk(id)),
                        retry_count: 0,
                    })
                    .await;
            }
        }
    }

    if !symbol_buffer.is_empty() {
        let batch = symbol_buffer;
        let _permit = state.db_semaphore.acquire().await;
        let ids = state
            .storage
            .create_code_symbols_batch(batch.clone())
            .await?;

        for (id, sym) in ids.iter().zip(batch.iter()) {
            let embed_text = sym
                .signature
                .clone()
                .unwrap_or_else(|| format!("{} {}", sym.symbol_type, sym.name));
            let _ = state
                .embedding_queue
                .send(EmbeddingRequest {
                    text: embed_text,
                    responder: None,
                    target: Some(EmbeddingTarget::Symbol(id.clone())),
                    retry_count: 0,
                })
                .await;
        }
    }

    // Final flush of remaining relations
    if !relation_buffer.is_empty() {
        let stats = create_symbol_relations(
            state.storage.as_ref(),
            project_id,
            &relation_buffer,
            &symbol_index,
        )
        .await;
        total_relation_stats.created += stats.created;
        total_relation_stats.failed += stats.failed;
        total_relation_stats.unresolved += stats.unresolved;
    }

    // Log relation stats
    if total_relation_stats.created > 0 || total_relation_stats.failed > 0 {
        tracing::info!(
            created = total_relation_stats.created,
            failed = total_relation_stats.failed,
            unresolved = total_relation_stats.unresolved,
            "Symbol relations indexed"
        );
    }

    status.status = IndexState::EmbeddingPending;
    status.completed_at = Some(crate::types::Datetime::default());

    state.storage.update_index_status(status.clone()).await?;

    // Rebuild in-memory BM25 index for this project from the freshly inserted chunks
    state
        .code_search
        .rebuild_from_storage(state.storage.as_ref(), project_id)
        .await;

    Ok(status)
}

/// Result of an incremental re-index operation.
#[derive(Debug, Default)]
pub struct IncrementalResult {
    /// New/updated chunks that were written to storage (without embedding yet).
    pub new_chunks: Vec<CodeChunk>,
    /// File paths that were removed from the index (file no longer exists on disk).
    pub deleted_files: Vec<String>,
    /// Number of files whose content changed and were re-indexed.
    pub updated_files: usize,
}

/// Incremental re-index for changed files only.
/// Returns an [`IncrementalResult`] describing what changed.
/// The caller is responsible for rebuilding in-memory indices (BM25 etc.) when needed.
pub async fn incremental_index(
    state: Arc<AppState>,
    project_id: &str,
    changed_paths: Vec<std::path::PathBuf>,
) -> Result<IncrementalResult> {
    let mut result = IncrementalResult::default();
    // Keep a local alias for the previous `updated` counter used inside the macro.
    macro_rules! inc_updated {
        () => {
            result.updated_files += 1;
        };
    }

    // Issue 4 fix: Bounded-concurrency parsing via JoinSet (same pattern as do_index_project).
    let max_concurrent_parses = MAX_CONCURRENT_PARSES;
    // Return type: (chunks, symbols, references, path_str, new_hash)
    type IncrResult = (
        Vec<CodeChunk>,
        Vec<CodeSymbol>,
        Vec<CodeReference>,
        String,
        String,
    );
    let mut parse_set: tokio::task::JoinSet<IncrResult> = tokio::task::JoinSet::new();

    // Process one completed incremental parse result (DB writes + relations + hash store).
    macro_rules! drain_one_incr {
        ($join_result:expr) => {{
            let (chunks, symbols, references, path_str, new_hash) = $join_result.map_err(|e| {
                crate::AppError::Internal(
                    format!("incremental parse/chunk panicked: {e}").into(),
                )
            })?;

            let _permit = state.db_semaphore.acquire().await;
            if let Ok(results) = state.storage.create_code_chunks_batch(chunks).await {
                for (id, chunk) in results {
                    // Collect the written chunk so the caller can inspect / BM25-rebuild selectively.
                    result.new_chunks.push(chunk.clone());
                    let _ = state
                        .embedding_queue
                        .send(EmbeddingRequest {
                            text: chunk.content,
                            responder: None,
                            target: Some(EmbeddingTarget::Chunk(id)),
                            retry_count: 0,
                        })
                        .await;
                }
            }

            if !symbols.is_empty() {
                let _permit = state.db_semaphore.acquire().await;
                let created_ids = match state
                    .storage
                    .create_code_symbols_batch(symbols.clone())
                    .await
                {
                    Ok(ids) => ids,
                    Err(e) => {
                        tracing::warn!(path = %path_str, error = %e, "Failed to create symbols");
                        vec![]
                    }
                };

                for (id, sym) in created_ids.iter().zip(symbols.iter()) {
                    if let Some(sig) = &sym.signature {
                        let _ = state
                            .embedding_queue
                            .send(EmbeddingRequest {
                                text: sig.clone(),
                                responder: None,
                                target: Some(EmbeddingTarget::Symbol(id.clone())),
                                retry_count: 0,
                            })
                            .await;
                    }
                }
            }

            // Create relations using project-wide symbol index for cross-file resolution
            // Also detect containment edges from symbol nesting within this file.
            let containment_refs = detect_containment_references(&symbols);
            let mut all_refs = references;
            all_refs.extend(containment_refs);

            if !all_refs.is_empty() {
                let mut symbol_index = SymbolIndex::new();
                if let Ok(all_symbols) = state.storage.get_project_symbols(project_id).await {
                    symbol_index.add_batch(&all_symbols);
                }
                symbol_index.add_batch(&symbols);
                let _stats = create_symbol_relations(
                    state.storage.as_ref(),
                    project_id,
                    &all_refs,
                    &symbol_index,
                )
                .await;
            }

            // Store updated file hash
            let _ = state
                .storage
                .set_file_hash(project_id, &path_str, &new_hash)
                .await;
            inc_updated!();
        }};
    }

    for path in changed_paths {
        let path_str = path.to_string_lossy().to_string();

        if crate::codebase::scanner::is_ignored_file(&path)
            || !crate::codebase::scanner::is_code_file(&path)
        {
            tracing::debug!(path = %path_str, "Skipping ignored/non-code file from watcher event");
            continue;
        }

        if !path.exists() {
            match state
                .storage
                .delete_chunks_by_path(project_id, &path_str)
                .await
            {
                Ok(deleted) => {
                    if deleted > 0 {
                        tracing::debug!(path = %path_str, deleted, "Removed chunks for deleted file");
                        result.deleted_files.push(path_str.clone());
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %path_str, error = %e, "Failed to delete chunks");
                }
            }
            // Also delete symbols and file hash
            let _ = state
                .storage
                .delete_symbols_by_path(project_id, &path_str)
                .await;
            let _ = state.storage.delete_file_hash(project_id, &path_str).await;
            continue;
        }

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path_str, error = %e, "Failed to read file");
                continue;
            }
        };

        let new_hash = blake3::hash(content.as_bytes()).to_hex().to_string();

        // Compare file-level hash from dedicated file_hashes table
        if let Ok(Some(existing_hash)) = state.storage.get_file_hash(project_id, &path_str).await {
            if existing_hash == new_hash {
                continue; // File unchanged, skip re-indexing
            }
        }

        let _ = state
            .storage
            .delete_chunks_by_path(project_id, &path_str)
            .await;
        let _ = state
            .storage
            .delete_symbols_by_path(project_id, &path_str)
            .await;

        // Issue 4 fix: Drain one completed parse before spawning if at capacity.
        if parse_set.len() >= max_concurrent_parses {
            if let Some(join_result) = parse_set.join_next().await {
                drain_one_incr!(join_result);
            }
        }

        // Spawn CPU-bound chunk+parse onto the blocking thread pool.
        // `content` is moved (not cloned) — the hash was already computed above.
        // `path_str` and `new_hash` are moved through the result for post-processing.
        let path_for_blocking = path.clone();
        let project_id_for_blocking = project_id.to_string();
        parse_set.spawn_blocking(move || {
            let chunks =
                super::chunker::chunk_file(&path_for_blocking, &content, &project_id_for_blocking);
            let (symbols, references) =
                CodeParser::parse_file(&path_for_blocking, &content, &project_id_for_blocking);
            (chunks, symbols, references, path_str, new_hash)
        });
    }

    // Drain remaining in-flight parse tasks
    while let Some(join_result) = parse_set.join_next().await {
        drain_one_incr!(join_result);
    }

    // NOTE: rebuild_from_storage is intentionally NOT called here.
    // The caller (CodebaseManager) decides when to rebuild the in-memory BM25
    // index, allowing it to batch multiple incremental results or defer the
    // rebuild to a background worker.

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TestContext;
    use std::fs;

    #[tokio::test]
    async fn test_indexer_batching() {
        let ctx = TestContext::new().await;
        let project_dir = ctx._temp_dir.path().join("test_project");
        fs::create_dir_all(&project_dir).unwrap();

        for i in 0..150 {
            let file_path = project_dir.join(format!("file_{}.rs", i));
            fs::write(file_path, format!("fn test_{}() {{}}", i)).unwrap();
        }

        // Must run with a real queue/worker setup or mock state
        // For unit test, we can just use the ctx.state which has a dummy queue if we updated TestContext
        // But TestContext::new() needs to be updated to initialize embedding_queue.

        let status = index_project(ctx.state.clone(), &project_dir)
            .await
            .unwrap();

        assert_eq!(status.total_files, 150);
        assert_eq!(status.total_chunks, 150);

        // Use in-memory BM25 engine (rebuilt automatically after indexing)
        let chunks = ctx
            .state
            .code_search
            .search("fn test", None, 200, ctx.state.storage.as_ref())
            .await;
        assert_eq!(chunks.len(), 150);
    }
}
