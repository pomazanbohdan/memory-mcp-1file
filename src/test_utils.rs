use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::OnceCell;

use crate::config::{AppConfig, AppState, STATUS_READY};
use crate::embedding::{
    AdaptiveEmbeddingQueue, EmbeddingConfig, EmbeddingMetrics, EmbeddingService, EmbeddingStore,
    ModelType,
};
use crate::storage::SurrealStorage;

pub struct TestContext {
    pub state: Arc<AppState>,
    pub _temp_dir: TempDir,
}

impl TestContext {
    pub async fn new() -> Self {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let db_path = temp_dir.path();

        let storage = SurrealStorage::new(db_path)
            .await
            .expect("Failed to init storage");

        let embedding_config = EmbeddingConfig {
            model: ModelType::Mock,
            cache_size: 100,
            batch_size: 10,
            cache_dir: None,
        };
        let embedding = Arc::new(EmbeddingService::new(embedding_config));
        embedding.start_loading();

        let mut attempts = 0;
        while !embedding.is_ready() {
            if attempts > 10 {
                panic!("Mock embedding service failed to start");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            attempts += 1;
        }

        let embedding_store =
            EmbeddingStore::new(db_path, "mock").expect("Failed to init embedding store");
        let metrics = Arc::new(EmbeddingMetrics::new());
        let (queue_tx, _queue_rx) = tokio::sync::mpsc::channel(1000);
        let adaptive_queue = AdaptiveEmbeddingQueue::with_defaults(queue_tx, metrics);

        let config = AppConfig {
            data_dir: db_path.to_path_buf(),
            model: "mock".to_string(),
            cache_size: 100,
            batch_size: 10,
            timeout_ms: 5000,
            log_level: "debug".to_string(),
        };

        let storage_cell = Arc::new(OnceCell::const_new());
        storage_cell.set(storage).ok();

        let embedding_store_cell = Arc::new(OnceCell::const_new());
        embedding_store_cell.set(embedding_store).ok();

        let state = Arc::new(AppState {
            config,
            storage: storage_cell,
            embedding,
            embedding_store: embedding_store_cell,
            embedding_queue: adaptive_queue,
            progress: crate::config::IndexProgressTracker::new(),
            db_semaphore: Arc::new(tokio::sync::Semaphore::new(10)),
            init_status: Arc::new(std::sync::atomic::AtomicU8::new(STATUS_READY)),
            init_error: Arc::new(tokio::sync::RwLock::new(None)),
        });

        Self {
            state,
            _temp_dir: temp_dir,
        }
    }
}
