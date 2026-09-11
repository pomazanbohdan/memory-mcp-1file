use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize};
use std::sync::Arc;

use tokio::sync::{watch, RwLock, Semaphore};

use crate::embedding::{AdaptiveEmbeddingQueue, EmbeddingService, EmbeddingStore, ModelType};
use crate::search::CodeSearchEngine;
use crate::storage::SurrealStorage;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub data_dir: PathBuf,
    pub model: String,
    pub cache_size: usize,
    pub batch_size: usize,
    pub timeout_ms: u64,
    pub log_level: String,
    /// Maximum time (ms) a tool call will block waiting for the embedding model
    /// to finish loading. This allows the MCP session to survive the first-time
    /// model download on a fresh machine without the client seeing a timeout.
    /// Default: 600_000 ms (10 minutes) — longer than any realistic download.
    pub model_load_timeout_ms: u64,

    // ── Embedding pipeline ────────────────────────────────────────────────────
    /// Capacity of the `tokio::sync::mpsc` channel used to buffer embedding
    /// requests before the worker picks them up. Higher values reduce
    /// back-pressure during ingestion bursts.
    /// Default: 256.
    pub embedding_queue_capacity: usize,
    /// Minimum number of pending embedding requests before the worker flushes
    /// a batch eagerly (without waiting for the deadline).
    /// Default: 8.
    pub embedding_batch_size: usize,

    // ── Codebase IndexWorker ──────────────────────────────────────────────────
    /// Maximum number of file-system events to accumulate before forcing an
    /// incremental-index flush. Larger values reduce DB round-trips on heavy
    /// save storms.
    /// Default: 20.
    pub index_batch_size: usize,
    /// Debounce window (ms): how long the IndexWorker waits for more jobs
    /// before flushing an incomplete batch.
    /// Default: 2 000 ms.
    pub index_debounce_ms: u64,
    /// How often (minutes) the periodic manifest-diff task runs to catch
    /// changes missed by the file-system watcher.
    /// Default: 10 minutes.
    pub manifest_diff_interval_mins: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            data_dir: dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("memory-mcp"),
            model: ModelType::default().to_string(),
            cache_size: 1000,
            batch_size: 8,
            timeout_ms: 30000,
            log_level: "info".to_string(),
            model_load_timeout_ms: 600_000,
            embedding_queue_capacity: 256,
            embedding_batch_size: 8,
            index_batch_size: 20,
            index_debounce_ms: 2_000,
            manifest_diff_interval_mins: 10,
        }
    }
}

pub struct IndexMonitor {
    pub total_files: AtomicU32,
    pub indexed_files: AtomicU32,
    pub current_file: std::sync::RwLock<String>,
}

impl Default for IndexMonitor {
    fn default() -> Self {
        Self {
            total_files: AtomicU32::new(0),
            indexed_files: AtomicU32::new(0),
            current_file: std::sync::RwLock::new(String::new()),
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
    pub storage: Arc<SurrealStorage>,
    pub embedding: Arc<EmbeddingService>,
    pub embedding_store: Arc<EmbeddingStore>,
    pub embedding_queue: AdaptiveEmbeddingQueue,
    pub progress: IndexProgressTracker,
    /// Semaphore to limit concurrent DB operations (prevents SurrealKV channel exhaustion)
    pub db_semaphore: Arc<Semaphore>,
    /// In-memory BM25 index for code chunks (replaces broken SurrealDB FTS)
    pub code_search: Arc<CodeSearchEngine>,
    /// Atomic lock: set of project IDs currently being (re-)indexed.
    /// Insert returns `false` if the ID is already present → concurrent request is rejected.
    /// Removed when indexing finishes (success or failure).
    pub indexing_projects: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Shutdown signal sender. Send `true` to request graceful shutdown of background loops.
    pub shutdown_tx: watch::Sender<bool>,
    /// Per-project pending job counters shared with [`IndexJobSender`] instances.
    ///
    /// Key = project_id.  Each `Arc<AtomicUsize>` is also held inside the
    /// corresponding `IndexJobSender` so both sides share the same counter
    /// without `AppState` needing to import the `codebase` crate.
    pub index_pending: Arc<RwLock<HashMap<String, Arc<AtomicUsize>>>>,
}

impl AppState {
    /// Returns a new receiver subscribed to the shutdown channel.
    /// Background tasks should select on `rx.changed()` and exit when `*rx.borrow() == true`.
    pub fn shutdown_rx(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }
}
