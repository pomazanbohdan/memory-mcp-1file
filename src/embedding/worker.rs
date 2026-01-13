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
    engine: Arc<tokio::sync::RwLock<Option<EmbeddingEngine>>>,
    store: Arc<tokio::sync::OnceCell<EmbeddingStore>>,
    state: Arc<crate::config::AppState>,
}

impl EmbeddingWorker {
    pub fn new(
        queue: mpsc::Receiver<EmbeddingRequest>,
        engine: Arc<tokio::sync::RwLock<Option<EmbeddingEngine>>>,
        store: Arc<tokio::sync::OnceCell<EmbeddingStore>>,
        state: Arc<crate::config::AppState>,
    ) -> Self {
        Self {
            queue,
            engine,
            store,
            state,
        }
    }

    pub async fn run(mut self) -> usize {
        let mut batch = Vec::with_capacity(32);
        let mut processed_count = 0;
        let deadline = tokio::time::sleep(Duration::from_millis(100));
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                biased;

                recv_result = self.queue.recv() => {
                    match recv_result {
                        Some(req) => {
                            batch.push(req);
                            if batch.len() >= 32 {
                                if self.process_batch(&mut batch).await {
                                    processed_count += 32;
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

        let guard = self.engine.read().await;
        let engine = match guard.as_ref() {
            Some(e) => e,
            None => {
                // Return false to indicate retry needed
                return false;
            }
        };

        let mut final_embeddings = Vec::with_capacity(batch.len());
        let mut misses_indices = Vec::new();
        let mut misses_texts = Vec::new();

        for (i, req) in batch.iter().enumerate() {
            let hash = blake3::hash(req.text.as_bytes()).to_hex().to_string();

            let cached = if let Some(store) = self.store.get() {
                store.get(&hash).await
            } else {
                None
            };

            if let Some(vec) = cached {
                final_embeddings.push(Some(vec));
            } else {
                final_embeddings.push(None);
                misses_indices.push(i);
                misses_texts.push(req.text.clone());
            }
        }

        if !misses_texts.is_empty() {
            match engine.embed_batch(&misses_texts) {
                Ok(new_embeddings) => {
                    for (local_idx, vec) in new_embeddings.into_iter().enumerate() {
                        let original_idx = misses_indices[local_idx];
                        let req = &batch[original_idx];
                        let hash = blake3::hash(req.text.as_bytes()).to_hex().to_string();

                        if let Some(store) = self.store.get() {
                            let _ = store.put(hash, vec.clone()).await;
                        }
                        final_embeddings[original_idx] = Some(vec);
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Batch embedding failed (items will have no embeddings): {}",
                        e
                    );
                    // Log retry info for monitoring - actual re-queue needs queue sender
                    for req in batch.iter() {
                        if req.retry_count < 3 {
                            tracing::warn!(
                                "Embedding failed for target {:?} (attempt {}/3)",
                                req.target,
                                req.retry_count + 1
                            );
                        }
                    }
                }
            }
        }

        // Collect updates for batch processing instead of spawning per item
        let mut symbol_updates: Vec<(String, Vec<f32>)> = Vec::new();
        let mut chunk_updates: Vec<(String, Vec<f32>)> = Vec::new();

        for (req, emb_opt) in batch.drain(..).zip(final_embeddings) {
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
            } else if let Some(tx) = req.responder {
                let _ = tx.send(vec![]);
            }
        }

        // Batch update instead of individual spawns
        use crate::storage::StorageBackend;

        if !symbol_updates.is_empty() {
            if let Some(storage) = self.state.storage() {
                if let Err(e) = storage
                    .batch_update_symbol_embeddings(&symbol_updates)
                    .await
                {
                    tracing::warn!(count = symbol_updates.len(), error = %e, "Batch symbol embedding update failed");
                }
            }
        }

        if !chunk_updates.is_empty() {
            if let Some(storage) = self.state.storage() {
                if let Err(e) = storage.batch_update_chunk_embeddings(&chunk_updates).await {
                    tracing::warn!(count = chunk_updates.len(), error = %e, "Batch chunk embedding update failed");
                }
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
        let storage = SurrealStorage::new(dir.path()).await.unwrap();
        let store = EmbeddingStore::new(dir.path(), "mock").unwrap();

        let config = EmbeddingConfig {
            model: ModelType::Mock,
            cache_size: 100,
            batch_size: 10,
            cache_dir: None,
        };
        let service = Arc::new(EmbeddingService::new(config));

        let (tx, rx) = mpsc::channel(100);
        let metrics = std::sync::Arc::new(EmbeddingMetrics::new());
        let adaptive_queue = AdaptiveEmbeddingQueue::with_defaults(tx, metrics);

        let storage_cell = Arc::new(tokio::sync::OnceCell::const_new());
        storage_cell.set(storage).ok();

        let store_cell = Arc::new(tokio::sync::OnceCell::const_new());
        store_cell.set(store).ok();

        let _worker = EmbeddingWorker::new(
            rx,
            service.get_engine(),
            store_cell.clone(),
            Arc::new(crate::config::AppState {
                config: crate::config::AppConfig::default(),
                storage: storage_cell,
                embedding: service,
                embedding_store: store_cell,
                embedding_queue: adaptive_queue,
                progress: crate::config::IndexProgressTracker::new(),
                db_semaphore: Arc::new(tokio::sync::Semaphore::new(10)),
                init_status: Arc::new(std::sync::atomic::AtomicU8::new(
                    crate::config::STATUS_READY,
                )),
                init_error: Arc::new(tokio::sync::RwLock::new(None)),
            }),
        );
    }
}
