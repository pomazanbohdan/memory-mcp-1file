use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use tokio::sync::{OnceCell, RwLock, Semaphore};

use crate::embedding::{AdaptiveEmbeddingQueue, EmbeddingService, EmbeddingStore};
use crate::storage::SurrealStorage;

/// Server initialization status constants
pub const STATUS_INITIALIZING: u8 = 0;
pub const STATUS_READY: u8 = 1;
pub const STATUS_ERROR: u8 = 2;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub data_dir: PathBuf,
    pub model: String,
    pub cache_size: usize,
    pub batch_size: usize,
    pub timeout_ms: u64,
    pub log_level: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            data_dir: dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("memory-mcp"),
            model: "e5_multi".to_string(),
            cache_size: 1000,
            batch_size: 8,
            timeout_ms: 30000,
            log_level: "info".to_string(),
        }
    }
}

pub struct IndexMonitor {
    pub total_files: AtomicU32,
    pub indexed_files: AtomicU32,
}

impl Default for IndexMonitor {
    fn default() -> Self {
        Self {
            total_files: AtomicU32::new(0),
            indexed_files: AtomicU32::new(0),
        }
    }
}

pub struct IndexProgressTracker {
    projects: RwLock<HashMap<String, Arc<IndexMonitor>>>,
}

impl IndexProgressTracker {
    pub fn new() -> Self {
        Self {
            projects: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get_or_create(&self, project_id: &str) -> Arc<IndexMonitor> {
        {
            let projects = self.projects.read().await;
            if let Some(monitor) = projects.get(project_id) {
                return monitor.clone();
            }
        }
        let mut projects = self.projects.write().await;
        projects
            .entry(project_id.to_string())
            .or_insert_with(|| Arc::new(IndexMonitor::default()))
            .clone()
    }

    pub async fn get(&self, project_id: &str) -> Option<Arc<IndexMonitor>> {
        self.projects.read().await.get(project_id).cloned()
    }

    pub async fn remove(&self, project_id: &str) {
        self.projects.write().await.remove(project_id);
    }
}

impl Default for IndexProgressTracker {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AppState {
    pub config: AppConfig,
    pub storage: Arc<OnceCell<SurrealStorage>>,
    pub embedding: Arc<EmbeddingService>,
    pub embedding_store: Arc<OnceCell<EmbeddingStore>>,
    pub embedding_queue: AdaptiveEmbeddingQueue,
    pub progress: IndexProgressTracker,
    pub db_semaphore: Arc<Semaphore>,
    pub init_status: Arc<AtomicU8>,
    pub init_error: Arc<RwLock<Option<String>>>,
}

impl AppState {
    pub fn is_ready(&self) -> bool {
        self.init_status.load(Ordering::Acquire) == STATUS_READY
    }

    pub fn status_code(&self) -> u8 {
        self.init_status.load(Ordering::Acquire)
    }

    pub fn storage(&self) -> Option<&SurrealStorage> {
        self.storage.get()
    }

    pub fn embedding_store(&self) -> Option<&EmbeddingStore> {
        self.embedding_store.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU8;

    fn create_test_init_status(status: u8) -> Arc<AtomicU8> {
        Arc::new(AtomicU8::new(status))
    }

    #[test]
    fn test_status_constants() {
        assert_eq!(STATUS_INITIALIZING, 0);
        assert_eq!(STATUS_READY, 1);
        assert_eq!(STATUS_ERROR, 2);
    }

    #[test]
    fn test_is_ready_when_initializing() {
        let status = create_test_init_status(STATUS_INITIALIZING);
        assert_ne!(status.load(Ordering::Acquire), STATUS_READY);
    }

    #[test]
    fn test_is_ready_when_ready() {
        let status = create_test_init_status(STATUS_READY);
        assert_eq!(status.load(Ordering::Acquire), STATUS_READY);
    }

    #[test]
    fn test_is_ready_when_error() {
        let status = create_test_init_status(STATUS_ERROR);
        assert_ne!(status.load(Ordering::Acquire), STATUS_READY);
    }

    #[test]
    fn test_status_code_returns_correct_value() {
        let status_init = create_test_init_status(STATUS_INITIALIZING);
        assert_eq!(status_init.load(Ordering::Acquire), STATUS_INITIALIZING);

        let status_ready = create_test_init_status(STATUS_READY);
        assert_eq!(status_ready.load(Ordering::Acquire), STATUS_READY);

        let status_error = create_test_init_status(STATUS_ERROR);
        assert_eq!(status_error.load(Ordering::Acquire), STATUS_ERROR);
    }

    #[test]
    fn test_app_config_default() {
        let config = AppConfig::default();
        assert_eq!(config.model, "e5_multi");
        assert_eq!(config.cache_size, 1000);
        assert_eq!(config.batch_size, 8);
        assert_eq!(config.timeout_ms, 30000);
        assert_eq!(config.log_level, "info");
    }

    #[tokio::test]
    async fn test_index_progress_tracker() {
        let tracker = IndexProgressTracker::new();

        let monitor1 = tracker.get_or_create("project1").await;
        monitor1.total_files.store(10, Ordering::Relaxed);
        monitor1.indexed_files.store(5, Ordering::Relaxed);

        let retrieved = tracker.get("project1").await;
        assert!(retrieved.is_some());
        let m = retrieved.unwrap();
        assert_eq!(m.total_files.load(Ordering::Relaxed), 10);
        assert_eq!(m.indexed_files.load(Ordering::Relaxed), 5);

        assert!(tracker.get("nonexistent").await.is_none());

        tracker.remove("project1").await;
        assert!(tracker.get("project1").await.is_none());
    }

    #[tokio::test]
    async fn test_index_monitor_default() {
        let monitor = IndexMonitor::default();
        assert_eq!(monitor.total_files.load(Ordering::Relaxed), 0);
        assert_eq!(monitor.indexed_files.load(Ordering::Relaxed), 0);
    }
}
