use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tracing::instrument;

use super::engine::EmbeddingEngine;
use super::store::EmbeddingStore;

#[derive(Debug)]
pub enum EmbeddingTarget {
    Symbol(String),
    Chunk(String),
}

pub struct EmbeddingRequest {
    pub text: String,
    pub responder: Option<oneshot::Sender<Vec<f32>>>,
    pub target: Option<EmbeddingTarget>,
    pub retry_count: u8,
}

pub struct EmbeddingWorker {
    queue: mpsc::Receiver<EmbeddingRequest>,
    requeue_tx: mpsc::Sender<EmbeddingRequest>,
    engine: Arc<tokio::sync::RwLock<Option<Arc<EmbeddingEngine>>>>,
    store: Arc<EmbeddingStore>,
    storage: Arc<crate::storage::SurrealStorage>,
    metrics: Arc<super::metrics::EmbeddingMetrics>,
}

impl EmbeddingWorker {
    pub fn new(
        queue: mpsc::Receiver<EmbeddingRequest>,
        requeue_tx: mpsc::Sender<EmbeddingRequest>,
        engine: Arc<tokio::sync::RwLock<Option<Arc<EmbeddingEngine>>>>,
        store: Arc<EmbeddingStore>,
        state: Arc<crate::config::AppState>,
    ) -> Self {
        let metrics = state.embedding_queue.metrics_arc();
        Self {
            queue,
            requeue_tx,
            engine,
            store,
            storage: state.storage.clone(),
            metrics,
        }
    }

    pub async fn run(mut self) -> usize {
        let mut batch = Vec::with_capacity(2);
        let mut processed_count = 0;
        let mut total_received = 0usize;
        let mut engine_not_ready_count = 0u64;
        let deadline = tokio::time::sleep(Duration::from_millis(100));
        tokio::pin!(deadline);

        tracing::info!("Embedding worker started, waiting for requests");

        loop {
            tokio::select! {
                biased;

                recv_result = self.queue.recv() => {
                    match recv_result {
                        Some(req) => {
                            total_received += 1;
                            if total_received == 1 || total_received.is_multiple_of(500) {
                                tracing::info!(total_received, batch_size = batch.len(), "Embedding worker receiving items");
                            }
                            batch.push(req);
                            if batch.len() >= 2 {
                                let n = batch.len();
                                if self.process_batch(&mut batch).await {
                                    processed_count += n;
                                } else {
                                    engine_not_ready_count += 1;
                                    if engine_not_ready_count <= 5
                                        || engine_not_ready_count.is_multiple_of(100)
                                    {
                                        tracing::warn!(engine_not_ready_count, batch_pending = batch.len(), "Engine not ready, items accumulating");
                                    }
                                }
                                deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(100));
                            }
                        }
                        None => {
                            if !batch.is_empty() {
                                let remaining = batch.len();
                                tracing::info!(remaining, "Draining remaining embedding requests");
                                if self.process_batch(&mut batch).await {
                                    processed_count += remaining;
                                }
                            }
                            tracing::info!(processed_count, "Embedding worker shutdown complete");
                            break;
                        }
                    }
                }

                _ = &mut deadline => {
                    if !batch.is_empty() {
                        let count = batch.len();
                        if self.process_batch(&mut batch).await {
                            processed_count += count;
                            if processed_count % 500 == 0 || processed_count <= 8 {
                                tracing::info!(processed_count, "Embedding worker progress (deadline flush)");
                            }
                        } else {
                            engine_not_ready_count += 1;
                            if engine_not_ready_count <= 5
                                || engine_not_ready_count.is_multiple_of(100)
                            {
                                tracing::warn!(engine_not_ready_count, batch_pending = batch.len(), "Engine not ready on deadline flush");
                            }
                        }
                    }
                    deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(100));
                }
            }
        }

        processed_count
    }

    #[instrument(skip(self, batch), fields(batch_size = batch.len()))]
    async fn process_batch(&self, batch: &mut Vec<EmbeddingRequest>) -> bool {
        if batch.is_empty() {
            return true;
        }

        // Filter out trivially short texts (e.g. "pub mod foo;") that produce
        // degenerate embeddings near the average token centroid. These chunks
        // still participate in BM25 search but are excluded from vector search.
        const MIN_EMBED_CHARS: usize = 50;
        let mut skipped_short = 0usize;
        for req in batch.iter_mut() {
            // IMPORTANT: short-text skipping applies only to code chunks.
            // Symbols are often short (e.g. "id", "new", "run"), and skipping
            // them keeps `embedding IS NONE`, which can block completion_monitor.
            let skip_short_chunk = matches!(req.target, Some(EmbeddingTarget::Chunk(_)))
                && req.text.len() < MIN_EMBED_CHARS;
            if skip_short_chunk {
                // Mark as "no embedding needed" by clearing target —
                // the DB update loop below will simply skip it.
                skipped_short += 1;
                req.target = None;
                if let Some(tx) = req.responder.take() {
                    let _ = tx.send(Vec::new());
                }
            }
        }
        // Remove skipped items so they don't go through inference
        if skipped_short > 0 {
            tracing::debug!(
                skipped_short,
                min_chars = MIN_EMBED_CHARS,
                "Skipped short chunks from embedding"
            );
            for _ in 0..skipped_short {
                self.metrics.dec_queue();
            }
            batch.retain(|r| r.target.is_some() || r.responder.is_some());
            if batch.is_empty() {
                return true;
            }
        }

        // ── PHASE 1: Brief lock — clone the inner Arc, drop guard immediately ──
        // This ensures writers (model hot-reload) are never blocked during
        // inference or across the .await points below.
        let engine: Arc<EmbeddingEngine> = {
            let guard = self.engine.read().await;
            match guard.as_ref() {
                Some(e) => Arc::clone(e),
                None => {
                    // Return false to indicate retry needed
                    return false;
                }
            }
        }; // guard dropped — writers unblocked

        // ── PHASE 2: Cache lookups (async, no lock held) ──
        let mut final_embeddings = Vec::with_capacity(batch.len());
        let mut misses_indices = Vec::new();
        let mut misses_texts = Vec::new();

        for (i, req) in batch.iter().enumerate() {
            let hash = blake3::hash(req.text.as_bytes()).to_hex().to_string();

            if let Some(vec) = self.store.get(&hash).await {
                final_embeddings.push(Some(vec));
            } else {
                final_embeddings.push(None);
                misses_indices.push(i);
                misses_texts.push(req.text.clone());
            }
        }

        // ── PHASE 3: Offload inference to blocking pool (no lock, no block_in_place) ──
        tracing::debug!(
            cache_hits = batch.len() - misses_texts.len(),
            cache_misses = misses_texts.len(),
            "Phase 2 cache results"
        );
        if !misses_texts.is_empty() {
            let engine_for_blocking = Arc::clone(&engine);
            let embed_result =
                tokio::task::spawn_blocking(move || engine_for_blocking.embed_batch(&misses_texts))
                    .await;

            match embed_result {
                Ok(Ok(new_embeddings)) => {
                    // Collect all cache misses for a single batched disk write
                    let mut cache_batch = Vec::with_capacity(new_embeddings.len());
                    for (local_idx, emb_opt) in new_embeddings.into_iter().enumerate() {
                        let original_idx = misses_indices[local_idx];
                        if let Some(vec) = emb_opt {
                            let req = &batch[original_idx];
                            let hash = blake3::hash(req.text.as_bytes()).to_hex().to_string();

                            cache_batch.push((hash, vec.clone()));
                            final_embeddings[original_idx] = Some(vec);
                        } else {
                            // Per-item failure (e.g. empty token sequence) — try re-queue
                            let req = &batch[original_idx];
                            if req.retry_count < 3 {
                                tracing::warn!(
                                    target = ?req.target,
                                    attempt = req.retry_count + 1,
                                    "Embedding returned None, re-queuing"
                                );
                                let retry = EmbeddingRequest {
                                    text: req.text.clone(),
                                    responder: None,
                                    target: match &req.target {
                                        Some(EmbeddingTarget::Symbol(id)) => {
                                            Some(EmbeddingTarget::Symbol(id.clone()))
                                        }
                                        Some(EmbeddingTarget::Chunk(id)) => {
                                            Some(EmbeddingTarget::Chunk(id.clone()))
                                        }
                                        None => None,
                                    },
                                    retry_count: req.retry_count + 1,
                                };
                                let _ = self.requeue_tx.try_send(retry);
                            } else {
                                tracing::error!(
                                    target = ?req.target,
                                    "Embedding permanently failed after 3 retries"
                                );
                                self.metrics.inc_failed(1);
                            }
                        }
                    }
                    let _ = self.store.put_batch(cache_batch).await;
                }
                Ok(Err(e)) => {
                    tracing::error!(
                        batch_size = batch.len(),
                        error = %e,
                        "Batch embedding failed, re-queuing retryable items"
                    );
                    for req in batch.iter() {
                        if req.retry_count < 3 {
                            tracing::warn!(
                                target = ?req.target,
                                attempt = req.retry_count + 1,
                                "Re-queuing after batch failure"
                            );
                            let retry = EmbeddingRequest {
                                text: req.text.clone(),
                                responder: None,
                                target: match &req.target {
                                    Some(EmbeddingTarget::Symbol(id)) => {
                                        Some(EmbeddingTarget::Symbol(id.clone()))
                                    }
                                    Some(EmbeddingTarget::Chunk(id)) => {
                                        Some(EmbeddingTarget::Chunk(id.clone()))
                                    }
                                    None => None,
                                },
                                retry_count: req.retry_count + 1,
                            };
                            let _ = self.requeue_tx.try_send(retry);
                        } else {
                            tracing::error!(
                                target = ?req.target,
                                "Embedding permanently failed after 3 retries"
                            );
                            self.metrics.inc_failed(1);
                        }
                    }
                }
                Err(join_err) => {
                    tracing::error!("embed_batch task panicked: {}", join_err);
                    // Panic = unrecoverable — mark all as failed
                    self.metrics.inc_failed(batch.len() as u64);
                }
            }
        }

        // ── PHASE 4: Collect updates for batch DB writes (no lock held) ──
        let mut symbol_updates: Vec<(String, Vec<f32>)> = Vec::new();
        let mut chunk_updates: Vec<(String, Vec<f32>)> = Vec::new();
        let mut null_count = 0usize;

        for (req, emb_opt) in batch.drain(..).zip(final_embeddings) {
            self.metrics.dec_queue();
            if let Some(emb) = emb_opt {
                if let Some(tx) = req.responder {
                    let _ = tx.send(emb.clone());
                }

                if let Some(target) = req.target {
                    match target {
                        EmbeddingTarget::Symbol(id) => {
                            symbol_updates.push((id, emb));
                        }
                        EmbeddingTarget::Chunk(id) => {
                            chunk_updates.push((id, emb));
                        }
                    }
                }
            } else {
                null_count += 1;
                if let Some(tx) = req.responder {
                    let _ = tx.send(vec![]);
                }
            }
        }

        tracing::info!(
            symbol_updates = symbol_updates.len(),
            chunk_updates = chunk_updates.len(),
            null_count,
            "Phase 4 collected DB updates"
        );

        // Batch update instead of individual spawns
        use crate::storage::StorageBackend;

        if !symbol_updates.is_empty() {
            if let Err(e) = self
                .storage
                .batch_update_symbol_embeddings(&symbol_updates)
                .await
            {
                tracing::warn!(count = symbol_updates.len(), error = %e, "Batch symbol embedding update failed");
            }
        }

        if !chunk_updates.is_empty() {
            if let Err(e) = self
                .storage
                .batch_update_chunk_embeddings(&chunk_updates)
                .await
            {
                tracing::warn!(count = chunk_updates.len(), error = %e, "Batch chunk embedding update failed");
            }
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::{
        AdaptiveEmbeddingQueue, EmbeddingConfig, EmbeddingMetrics, EmbeddingService, ModelType,
    };
    use crate::storage::SurrealStorage;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_worker_initialization() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(SurrealStorage::new(dir.path(), 768).await.unwrap());
        let store = Arc::new(EmbeddingStore::new(dir.path(), "mock").unwrap());

        let config = EmbeddingConfig {
            model: ModelType::Mock,
            mrl_dim: None,
            cache_size: 100,
            batch_size: 10,
            cache_dir: None,
        };
        let service = Arc::new(EmbeddingService::new(config));

        let (tx, rx) = mpsc::channel(100);
        let metrics = std::sync::Arc::new(EmbeddingMetrics::new());
        let adaptive_queue = AdaptiveEmbeddingQueue::with_defaults(tx.clone(), metrics);

        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        let _worker = EmbeddingWorker::new(
            rx,
            tx,
            service.get_engine(),
            store.clone(),
            Arc::new(crate::config::AppState {
                config: crate::config::AppConfig::default(),
                storage,
                embedding: service,
                embedding_store: store,
                embedding_queue: adaptive_queue,
                progress: crate::config::IndexProgressTracker::new(),
                db_semaphore: Arc::new(tokio::sync::Semaphore::new(10)),
                code_search: Arc::new(crate::search::CodeSearchEngine::new()),
                indexing_projects: Arc::new(
                    std::sync::Mutex::new(std::collections::HashSet::new()),
                ),
                shutdown_tx,
                index_pending: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            }),
        );
    }
}
