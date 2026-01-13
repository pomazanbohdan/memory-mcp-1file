use std::sync::Arc;
use std::time::Duration;

use crate::config::AppState;
use crate::storage::StorageBackend;
use crate::types::IndexState;

const POLL_INTERVAL_SECS: u64 = 10;

pub async fn run_completion_monitor(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let storage = match state.storage() {
            Some(s) => s,
            None => continue,
        };

        let projects = match storage.list_projects().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Completion monitor: failed to list projects: {}", e);
                continue;
            }
        };

        for project_id in projects {
            if let Err(e) = check_and_complete_project(&state, &project_id).await {
                tracing::debug!(
                    project_id = %project_id,
                    error = %e,
                    "Completion check failed"
                );
            }
        }
    }
}

async fn check_and_complete_project(state: &Arc<AppState>, project_id: &str) -> crate::Result<()> {
    let storage = state
        .storage()
        .ok_or_else(|| crate::AppError::Storage("Storage not initialized".to_string()))?;

    let status = match storage.get_index_status(project_id).await? {
        Some(s) => s,
        None => return Ok(()),
    };

    if status.status != IndexState::EmbeddingPending {
        return Ok(());
    }

    let total_chunks = storage.count_chunks(project_id).await?;
    let total_symbols = storage.count_symbols(project_id).await?;
    let embedded_chunks = storage.count_embedded_chunks(project_id).await?;
    let embedded_symbols = storage.count_embedded_symbols(project_id).await?;

    let chunks_complete = embedded_chunks >= total_chunks;
    let symbols_complete = embedded_symbols >= total_symbols;
    let has_content = total_chunks > 0 || total_symbols > 0;

    if chunks_complete && symbols_complete && has_content {
        let mut updated_status = status.clone();
        updated_status.status = IndexState::Completed;
        updated_status.total_chunks = total_chunks;
        updated_status.total_symbols = total_symbols;

        storage.update_index_status(updated_status).await?;

        tracing::info!(
            project_id = %project_id,
            chunks = total_chunks,
            symbols = total_symbols,
            "Project indexing completed"
        );
    }

    Ok(())
}
