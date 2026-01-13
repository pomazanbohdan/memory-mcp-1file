use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::config::AppState;
use crate::storage::StorageBackend;
use crate::types::IndexState;
use crate::Result;

use super::indexer::index_project;
use super::watcher::FileWatcher;

pub struct CodebaseManager {
    state: Arc<AppState>,
    project_path: PathBuf,
    project_id: String,
    watcher: RwLock<Option<FileWatcher>>,
}

impl CodebaseManager {
    pub fn new(state: Arc<AppState>, project_path: PathBuf) -> Self {
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
        }
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub async fn start(&self) -> Result<()> {
        info!(project_id = %self.project_id, "Starting codebase manager");

        let storage = match self.state.storage() {
            Some(s) => s,
            None => {
                warn!("Storage not ready, deferring codebase manager start");
                return Ok(());
            }
        };

        let status = storage.get_index_status(&self.project_id).await?;

        match status {
            None => {
                info!("No index found, starting full indexing...");
                self.spawn_full_index();
            }
            Some(s) if s.status == IndexState::Completed => {
                info!("Index exists, will use watcher for updates");
            }
            Some(s) if s.status == IndexState::Indexing => {
                warn!("Previous indexing was interrupted, restarting...");
                self.spawn_full_index();
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

    fn spawn_full_index(&self) {
        let state = self.state.clone();
        let path = self.project_path.clone();

        tokio::spawn(async move {
            info!("Background indexing started");
            match index_project(state, &path).await {
                Ok(status) => {
                    info!(
                        files = status.indexed_files,
                        chunks = status.total_chunks,
                        "Background indexing completed"
                    );
                }
                Err(e) => {
                    error!("Background indexing failed: {}", e);
                }
            }
        });
    }

    async fn start_watcher(&self) -> Result<()> {
        let mut watcher = FileWatcher::new(vec![self.project_path.clone()]);

        let state = self.state.clone();
        let project_id = self.project_id.clone();

        watcher.start(move |changed_paths| {
            let state = state.clone();
            let project_id = project_id.clone();

            tokio::spawn(async move {
                info!(
                    count = changed_paths.len(),
                    "File changes detected, running incremental index"
                );
                match super::indexer::incremental_index(state, &project_id, changed_paths).await {
                    Ok(updated) => {
                        if updated > 0 {
                            info!(updated, "Incremental index completed");
                        }
                    }
                    Err(e) => {
                        error!("Incremental index failed: {}", e);
                    }
                }
            });
        })?;

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
