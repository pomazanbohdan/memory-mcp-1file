use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::config::AppState;
use crate::embedding::{EmbeddingRequest, EmbeddingTarget};
use crate::storage::StorageBackend;
use crate::types::IndexState;
use crate::Result;

use super::index_worker::{IndexJob, IndexJobSender};
use super::indexer::{index_project, IncrementalResult};
use super::scanner::scan_directory;
use super::watcher::FileWatcher;

/// Bump this when embedding model or pooling strategy changes.
/// On startup, if the stored version is lower, all embeddings are cleared
/// and re-generated via the existing resume pipeline.
const EMBEDDING_VERSION: u32 = 3; // v1: last-token, v2: mean pooling, v3: +short chunk filter

pub struct CodebaseManager {
    state: Arc<AppState>,
    project_path: PathBuf,
    project_id: String,
    watcher: RwLock<Option<FileWatcher>>,
    /// Sender to the background [`IndexWorker`] for this project.
    index_tx: IndexJobSender,
}

impl CodebaseManager {
    pub fn new(state: Arc<AppState>, project_path: PathBuf, index_tx: IndexJobSender) -> Self {
        let project_id = project_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        Self {
            state,
            project_path,
            project_id,
            watcher: RwLock::new(None),
            index_tx,
        }
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// After a successful full index, write every indexed file path into the
    /// `file_manifest` table so future validation can detect deletions.
    pub async fn bootstrap_manifest(&self) -> Result<()> {
        let project_path = self.project_path.clone();
        let file_paths = tokio::task::spawn_blocking(move || scan_directory(&project_path))
            .await
            .map_err(|e| {
                crate::AppError::Internal(format!("bootstrap_manifest scan panicked: {e}").into())
            })??;

        let path_strings: Vec<String> = file_paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        if path_strings.is_empty() {
            return Ok(());
        }

        self.state
            .storage
            .upsert_manifest_entries(&self.project_id, &path_strings)
            .await?;

        info!(
            project_id = %self.project_id,
            files = path_strings.len(),
            "File manifest bootstrapped"
        );
        Ok(())
    }

    /// Compare the stored file manifest against what is currently on disk and
    /// push discovered changes into the [`IndexWorker`] channel.
    ///
    /// * Files present in the manifest but missing on disk → `IndexJob::Delete`
    /// * Files on disk but missing in the manifest → `IndexJob::Upsert`
    /// * Files whose content might have changed (all current files) → `IndexJob::Upsert`
    ///   (the worker's `incremental_index` call will skip unchanged ones via hash comparison)
    ///
    /// Returns the number of jobs enqueued.
    pub async fn validate_index_full(&self) -> Result<IncrementalResult> {
        let project_id = &self.project_id;
        let status = self.state.storage.get_index_status(project_id).await?;

        // If the project has no persisted index status, treat it as not indexed
        // and skip background validation/re-indexing. This prevents periodic
        // auto-index from recreating chunks after explicit project deletion.
        if status.is_none() {
            debug!(
                project_id = %project_id,
                "validate_index_full: skipped (no index status)"
            );
            return Ok(IncrementalResult::default());
        }

        // 1. Scan the current directory.
        let project_path = self.project_path.clone();
        let current_files = tokio::task::spawn_blocking(move || scan_directory(&project_path))
            .await
            .map_err(|e| {
                crate::AppError::Internal(format!("validate_index_full scan panicked: {e}").into())
            })??;

        let current_set: std::collections::HashSet<String> = current_files
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        // 2. Load the stored manifest.
        let manifest = self.state.storage.get_manifest_entries(project_id).await?;
        let manifest_set: std::collections::HashSet<String> =
            manifest.iter().map(|e| e.file_path.clone()).collect();

        // 3. Deleted files: in manifest but not on disk → Delete jobs.
        let deleted: Vec<String> = manifest_set.difference(&current_set).cloned().collect();

        for path_str in &deleted {
            let path = PathBuf::from(path_str);
            if let Err(e) = self.index_tx.send(IndexJob::Delete(path)) {
                warn!(
                    project_id = %project_id,
                    path = %path_str,
                    error = %e,
                    "validate_index_full: failed to enqueue Delete job"
                );
            }
        }

        // 4. All current files → Upsert jobs.
        //    The worker calls `incremental_index` which skips files whose hash
        //    hasn't changed, so it is safe to enqueue all files.
        let mut enqueued_upserts = 0usize;
        for path in &current_files {
            if let Err(e) = self.index_tx.send(IndexJob::Upsert(path.clone())) {
                warn!(
                    project_id = %project_id,
                    path = ?path,
                    error = %e,
                    "validate_index_full: failed to enqueue Upsert job"
                );
            } else {
                enqueued_upserts += 1;
            }
        }

        info!(
            project_id = %project_id,
            deletes = deleted.len(),
            upserts = enqueued_upserts,
            "validate_index_full: jobs enqueued in IndexWorker"
        );

        // Return an empty IncrementalResult — the actual work happens
        // asynchronously inside the IndexWorker.
        Ok(IncrementalResult::default())
    }

    /// Start auto-indexing and file watching
    pub async fn start(&self) -> Result<()> {
        info!(project_id = %self.project_id, "Starting codebase manager");

        let status = self
            .state
            .storage
            .get_index_status(&self.project_id)
            .await?;

        match status {
            None => {
                info!("No index found, starting full indexing...");
                self.spawn_full_index();
            }
            Some(s)
                if s.status == IndexState::Completed
                    || s.status == IndexState::EmbeddingPending =>
            {
                // Check if embedding version changed (e.g. pooling strategy update)
                if s.embedding_version < EMBEDDING_VERSION {
                    warn!(
                        old_version = s.embedding_version,
                        new_version = EMBEDDING_VERSION,
                        "Embedding version mismatch — clearing old embeddings for re-generation"
                    );
                    let state = self.state.clone();
                    let project_path = self.project_path.clone();
                    let project_id = self.project_id.clone();
                    let index_tx = self.index_tx.clone();
                    let mut migration_status = s.clone();
                    tokio::spawn(async move {
                        // 1. Clear all embeddings (set to NONE)
                        match state.storage.clear_project_embeddings(&project_id).await {
                            Ok(_) => {
                                info!(project_id = %project_id, "Cleared old embeddings for re-generation");
                            }
                            Err(e) => {
                                error!("Failed to clear embeddings: {}", e);
                                return;
                            }
                        }
                        // 2. Set status to EmbeddingPending with new version
                        migration_status.status = IndexState::EmbeddingPending;
                        migration_status.embedding_version = EMBEDDING_VERSION;
                        if let Err(e) = state.storage.update_index_status(migration_status).await {
                            error!("Failed to update index status after migration: {}", e);
                            return;
                        }
                        // 3. Resume embeddings via existing pipeline
                        let mgr = CodebaseManager {
                            state: state.clone(),
                            project_path,
                            project_id,
                            watcher: RwLock::new(None),
                            index_tx,
                        };
                        match mgr.resume_embeddings().await {
                            Ok(count) => {
                                info!(count, "Embedding migration: enqueued items for re-embedding");
                            }
                            Err(e) => {
                                error!("Embedding migration resume failed: {}", e);
                            }
                        }
                    });
                } else {
                    info!(status = %s.status, "Index exists, validating against disk...");
                    // Spawn validation so we don't block start().
                    let state = self.state.clone();
                    let project_path = self.project_path.clone();
                    let project_id = self.project_id.clone();
                    let index_tx = self.index_tx.clone();
                    tokio::spawn(async move {
                        // Temporarily construct a manager-like helper for the spawn.
                        let mgr = CodebaseManager {
                            state: state.clone(),
                            project_path,
                            project_id: project_id.clone(),
                            watcher: RwLock::new(None),
                            index_tx,
                        };
                        match mgr.validate_index_full().await {
                            Ok(_) => {
                                info!("Background validation: jobs enqueued in IndexWorker");
                            }
                            Err(e) => {
                                error!("Background validation failed: {}", e);
                            }
                        }
                        // Check for orphaned unembedded chunks (e.g. container killed
                        // mid-embedding on a previous run) and resume if needed.
                        match mgr.resume_embeddings().await {
                            Ok(count) if count > 0 => {
                                info!(count, "Resumed orphaned embeddings from previous run");
                                // Set status to EmbeddingPending so completion_monitor
                                // detects when all embeddings finish and rebuilds HNSW.
                                if let Ok(Some(mut st)) = state.storage.get_index_status(&project_id).await {
                                    st.status = IndexState::EmbeddingPending;
                                    if let Err(e) = state.storage.update_index_status(st).await {
                                        error!("Failed to set EmbeddingPending after resume: {}", e);
                                    }
                                }
                            }
                            Ok(_) => {
                                debug!("No orphaned embeddings to resume");
                            }
                            Err(e) => {
                                error!("Failed to resume orphaned embeddings: {}", e);
                            }
                        }
                    });
                }
            }
            Some(s) if s.status == IndexState::Indexing => {
                warn!("Previous indexing was interrupted, resuming...");
                // Instead of destructive restart, validate files and resume embeddings
                let state = self.state.clone();
                let project_path = self.project_path.clone();
                let project_id = self.project_id.clone();
                let index_tx = self.index_tx.clone();
                let mut resume_status = s.clone();
                tokio::spawn(async move {
                    let mgr = CodebaseManager {
                        state: state.clone(),
                        project_path,
                        project_id,
                        watcher: RwLock::new(None),
                        index_tx,
                    };
                    // First: check for any new/changed/deleted files
                    if let Err(e) = mgr.validate_index_full().await {
                        error!("Failed to validate index during resume: {}", e);
                    }
                    // Then: re-enqueue any chunks/symbols missing embeddings
                    match mgr.resume_embeddings().await {
                        Ok(count) => {
                            if count > 0 {
                                info!("Resumed embedding for {} items", count);
                            } else {
                                info!("No unembedded items found, marking as EmbeddingPending");
                            }
                            // Set to EmbeddingPending so completion_monitor picks it up
                            resume_status.status = IndexState::EmbeddingPending;
                            if let Err(e) = mgr.state.storage.update_index_status(resume_status).await {
                                error!("Failed to update index status: {}", e);
                            }
                        }
                        Err(e) => {
                            error!("Failed to resume embeddings: {}", e);
                            // Fall back to full re-index only on error
                            mgr.spawn_full_index();
                        }
                    }
                });
            }
            Some(s) if s.status == IndexState::Failed => {
                warn!("Previous indexing failed, restarting...");
                self.spawn_full_index();
            }
            _ => {}
        }

        self.start_watcher().await?;

        Ok(())
    }

    /// Resume embedding for chunks/symbols that were indexed but not yet embedded.
    /// Used when recovering from an interrupted indexing session.
    async fn resume_embeddings(&self) -> Result<usize> {
        let storage = &self.state.storage;
        let queue = &self.state.embedding_queue;
        let mut enqueued = 0;

        // Re-enqueue unembedded chunks
        let unembedded_chunks = storage.get_unembedded_chunks(&self.project_id).await?;
        for (id, content) in &unembedded_chunks {
            let req = EmbeddingRequest {
                text: content.clone(),
                responder: None,
                target: Some(EmbeddingTarget::Chunk(id.clone())),
                retry_count: 0,
            };
            if let Err(e) = queue.send(req).await {
                tracing::warn!(chunk_id = %id, error = %e, "Failed to enqueue chunk embedding during resume");
            } else {
                enqueued += 1;
            }
        }

        // Re-enqueue unembedded symbols
        let unembedded_symbols = storage.get_unembedded_symbols(&self.project_id).await?;
        for (id, text) in &unembedded_symbols {
            let req = EmbeddingRequest {
                text: text.clone(),
                responder: None,
                target: Some(EmbeddingTarget::Symbol(id.clone())),
                retry_count: 0,
            };
            if let Err(e) = queue.send(req).await {
                tracing::warn!(symbol_id = %id, error = %e, "Failed to enqueue symbol embedding during resume");
            } else {
                enqueued += 1;
            }
        }

        Ok(enqueued)
    }

    fn spawn_full_index(&self) {
        let state = self.state.clone();
        let path = self.project_path.clone();
        let project_path2 = self.project_path.clone();
        let project_id = self.project_id.clone();
        let index_tx = self.index_tx.clone();

        tokio::spawn(async move {
            info!("Background indexing started");
            match index_project(state.clone(), &path).await {
                Ok(status) => {
                    info!(
                        files = status.indexed_files,
                        chunks = status.total_chunks,
                        "Background indexing completed"
                    );
                    // Bootstrap the file manifest after a successful full index.
                    let mgr = CodebaseManager {
                        state: state.clone(),
                        project_path: project_path2,
                        project_id,
                        watcher: RwLock::new(None),
                        index_tx,
                    };
                    if let Err(e) = mgr.bootstrap_manifest().await {
                        warn!("Failed to bootstrap file manifest: {}", e);
                    }
                    // Rebuild BM25 index now that all chunks are in storage.
                    state
                        .code_search
                        .rebuild_from_storage(state.storage.as_ref(), &mgr.project_id)
                        .await;
                }
                Err(e) => {
                    error!("Background indexing failed: {}", e);
                }
            }
        });
    }

    async fn start_watcher(&self) -> Result<()> {
        let mut watcher = FileWatcher::new(vec![self.project_path.clone()]);

        let index_tx = self.index_tx.clone();
        let project_id = self.project_id.clone();
        let shutdown_rx = self.state.shutdown_rx();

        watcher.start(
            move |changed_paths| {
                let tx = index_tx.clone();
                let pid = project_id.clone();
                for path in changed_paths {
                    // Determine whether the file still exists to decide job type.
                    let job = if path.exists() {
                        IndexJob::Upsert(path)
                    } else {
                        IndexJob::Delete(path)
                    };
                    if let Err(e) = tx.send(job) {
                        warn!(project_id = %pid, error = %e, "Watcher: failed to enqueue IndexJob");
                    }
                }
            },
            shutdown_rx,
        )?;

        *self.watcher.write().await = Some(watcher);
        info!(path = ?self.project_path, "File watcher started");

        Ok(())
    }

    pub async fn stop(&self) {
        if let Some(mut watcher) = self.watcher.write().await.take() {
            watcher.stop();
            info!("Codebase manager stopped");
        }
    }
}
