use std::path::Path;
use std::sync::Arc;

use tokio::fs;

use crate::config::AppState;
use crate::storage::StorageBackend;
use crate::types::{CodeChunk, CodeSymbol, IndexState, IndexStatus};
use crate::Result;

use super::chunker::chunk_file;
use super::parser::CodeParser;
use super::relations::{create_symbol_relations, RelationStats};
use super::scanner::scan_directory;
use super::symbol_index::SymbolIndex;

use crate::embedding::{EmbeddingRequest, EmbeddingTarget};
use crate::types::symbol::CodeReference;

pub async fn index_project(state: Arc<AppState>, project_path: &Path) -> Result<IndexStatus> {
    let storage = state
        .storage()
        .ok_or_else(|| crate::AppError::Storage("Storage not initialized".to_string()))?;

    let project_id = project_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let mut status = IndexStatus::new(project_id.clone());
    let monitor = state.progress.get_or_create(&project_id).await;

    storage.delete_project_chunks(&project_id).await?;
    storage.delete_project_symbols(&project_id).await?;

    let files = scan_directory(project_path)?;
    status.total_files = files.len() as u32;
    monitor
        .total_files
        .store(status.total_files, std::sync::atomic::Ordering::Relaxed);
    monitor
        .indexed_files
        .store(0, std::sync::atomic::Ordering::Relaxed);

    storage.update_index_status(status.clone()).await?;

    let batch_size = 20;
    let mut chunk_buffer = Vec::with_capacity(batch_size);
    let mut symbol_buffer = Vec::with_capacity(batch_size);
    let mut symbol_index = SymbolIndex::new();
    let mut relation_buffer: Vec<CodeReference> = Vec::new();
    let mut total_relation_stats = RelationStats::default();

    for file_path in &files {
        let content = match fs::read_to_string(file_path).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read file {:?}: {}", file_path, e);
                continue;
            }
        };

        // 1. Chunking (Vector Search)
        let chunks = chunk_file(file_path, &content, &project_id);
        for chunk in chunks {
            chunk_buffer.push(chunk);
            status.total_chunks += 1;

            if chunk_buffer.len() >= batch_size {
                let batch = std::mem::take(&mut chunk_buffer);
                let _permit = state.db_semaphore.acquire().await;
                if let Ok(results) = storage.create_code_chunks_batch(batch).await {
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
        }

        // 2. Parsing (Code Graph)
        let (symbols, references) = CodeParser::parse_file(file_path, &content, &project_id);

        if !symbols.is_empty() {
            tracing::debug!("File {:?}: found {} symbols", file_path, symbols.len());
        }

        // Add symbols to in-memory index FIRST (for relation resolution)
        for symbol in &symbols {
            symbol_index.add(symbol);
        }

        for symbol in symbols {
            symbol_buffer.push(symbol);
            status.total_symbols += 1;

            if symbol_buffer.len() >= batch_size {
                let batch: Vec<CodeSymbol> = std::mem::take(&mut symbol_buffer);
                let _permit = state.db_semaphore.acquire().await;
                if let Ok(ids) = storage.create_code_symbols_batch(batch.clone()).await {
                    let ids: Vec<String> = ids;
                    for (id, sym) in ids.iter().zip(batch.iter()) {
                        if let Some(sig) = &sym.signature {
                            let text: String = sig.clone();
                            let id_str: String = id.clone();
                            let _ = state
                                .embedding_queue
                                .send(EmbeddingRequest {
                                    text,
                                    responder: None,
                                    target: Some(EmbeddingTarget::Symbol(id_str)),
                                    retry_count: 0,
                                })
                                .await;
                        }
                    }
                }

                if !relation_buffer.is_empty() {
                    let refs = std::mem::take(&mut relation_buffer);
                    let stats =
                        create_symbol_relations(storage, &project_id, &refs, &symbol_index).await;
                    total_relation_stats.created += stats.created;
                    total_relation_stats.failed += stats.failed;
                    total_relation_stats.unresolved += stats.unresolved;
                }
            }
        }

        // Buffer references for deferred processing (after symbols are in DB)
        relation_buffer.extend(references);

        status.indexed_files += 1;
        monitor
            .indexed_files
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        if status.indexed_files.is_multiple_of(10) {
            if let Err(e) = storage.update_index_status(status.clone()).await {
                tracing::warn!("Failed to update intermediate status: {}", e);
            }
        }
    }

    if !chunk_buffer.is_empty() {
        let _permit = state.db_semaphore.acquire().await;
        if let Ok(results) = storage.create_code_chunks_batch(chunk_buffer).await {
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
        let ids: Vec<String> = storage.create_code_symbols_batch(batch.clone()).await?;

        for (id, sym) in ids.iter().zip(batch.iter()) {
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

    if !relation_buffer.is_empty() {
        let stats =
            create_symbol_relations(storage, &project_id, &relation_buffer, &symbol_index).await;
        total_relation_stats.created += stats.created;
        total_relation_stats.failed += stats.failed;
        total_relation_stats.unresolved += stats.unresolved;
    }

    if total_relation_stats.created > 0 || total_relation_stats.failed > 0 {
        tracing::info!(
            created = total_relation_stats.created,
            failed = total_relation_stats.failed,
            unresolved = total_relation_stats.unresolved,
            "Symbol relations indexed"
        );
    }

    status.status = IndexState::EmbeddingPending;
    status.completed_at = Some(surrealdb::sql::Datetime::default());

    storage.update_index_status(status.clone()).await?;

    Ok(status)
}

pub async fn incremental_index(
    state: Arc<AppState>,
    project_id: &str,
    changed_paths: Vec<std::path::PathBuf>,
) -> Result<usize> {
    let storage = state
        .storage()
        .ok_or_else(|| crate::AppError::Storage("Storage not initialized".to_string()))?;

    let mut updated = 0;

    for path in changed_paths {
        let path_str = path.to_string_lossy().to_string();

        if !path.exists() {
            match storage.delete_chunks_by_path(project_id, &path_str).await {
                Ok(deleted) => {
                    if deleted > 0 {
                        tracing::debug!(path = %path_str, deleted, "Removed chunks for deleted file");
                        updated += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %path_str, error = %e, "Failed to delete chunks");
                }
            }
            let _ = storage.delete_symbols_by_path(project_id, &path_str).await;
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

        let existing_chunks: Vec<CodeChunk> = storage
            .get_chunks_by_path(project_id, &path_str)
            .await
            .unwrap_or_else(|_| vec![]);

        if let Some(first_chunk) = existing_chunks.first() {
            if first_chunk.content_hash == new_hash {
                continue;
            }
        }

        let _ = storage.delete_chunks_by_path(project_id, &path_str).await;
        let _ = storage.delete_symbols_by_path(project_id, &path_str).await;

        let chunks = super::chunker::chunk_file(&path, &content, project_id);

        let _permit = state.db_semaphore.acquire().await;
        if let Ok(results) = storage.create_code_chunks_batch(chunks).await {
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

        let (symbols, references) = CodeParser::parse_file(&path, &content, project_id);
        if !symbols.is_empty() {
            let _permit = state.db_semaphore.acquire().await;
            let created_ids: Vec<String> =
                match storage.create_code_symbols_batch(symbols.clone()).await {
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

        if !references.is_empty() {
            let mut symbol_index = SymbolIndex::new();
            symbol_index.add_batch(&symbols);
            let _stats =
                create_symbol_relations(storage, project_id, &references, &symbol_index).await;
        }

        updated += 1;
    }

    Ok(updated)
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

        let status = index_project(ctx.state.clone(), &project_dir)
            .await
            .unwrap();

        assert_eq!(status.total_files, 150);
        assert_eq!(status.total_chunks, 150);

        let storage = ctx.state.storage().unwrap();
        let chunks = storage
            .bm25_search_code("fn test", None, 200)
            .await
            .unwrap();
        assert_eq!(chunks.len(), 150);
    }
}
