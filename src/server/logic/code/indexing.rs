use std::sync::Arc;

use rmcp::model::CallToolResult;
use serde_json::json;

use crate::config::AppState;
use crate::server::params::{
    DeleteProjectParams, GetIndexStatusParams, GetProjectStatsParams, IndexProjectParams,
    ListProjectsParams,
};
use crate::storage::StorageBackend;

use super::super::{error_response, success_json};

pub async fn index_project(
    state: &Arc<AppState>,
    params: IndexProjectParams,
) -> anyhow::Result<CallToolResult> {
    let path = std::path::Path::new(&params.path);

    if !path.exists() {
        return Ok(error_response(format!(
            "Path does not exist: {}",
            params.path
        )));
    }

    let project_id = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let force = params.force.unwrap_or(false);

    // Check current status from DB (for informational purposes / force flag)
    if let Ok(Some(status)) = state.storage.get_index_status(&project_id).await {
        match status.status {
            crate::types::IndexState::Completed | crate::types::IndexState::EmbeddingPending => {
                if !force {
                    return Ok(success_json(json!({
                        "project_id": project_id,
                        "status": status.status.to_string(),
                        "total_files": status.total_files,
                        "indexed_files": status.indexed_files,
                        "total_chunks": status.total_chunks,
                        "message": "Project already indexed. File changes are tracked incrementally. Use force=true to re-index from scratch."
                    })));
                }
                tracing::info!(project_id = %project_id, "Force re-indexing project");
            }
            crate::types::IndexState::Failed => {
                tracing::info!(
                    project_id = %project_id,
                    error = ?status.error_message,
                    "Previous indexing failed, re-indexing"
                );
            }
            crate::types::IndexState::Indexing => {
                // Will be caught by in-memory atomic lock below; DB state may lag
            }
        }
    }

    // Atomic TOCTOU guard: insert returns false if project_id already present.
    // This is the authoritative gate — the DB status check above is only informational.
    // We must NOT hold the MutexGuard across any `.await` point (std::sync::Mutex is !Send).
    // So: lock → check+insert → drop guard → then await if needed.
    let already_indexing = {
        let mut guard = state
            .indexing_projects
            .lock()
            .expect("indexing_projects mutex poisoned");
        !guard.insert(project_id.clone())
    }; // guard dropped here — no awaits held

    if already_indexing {
        // Already indexing — return current progress from DB
        let (total_files, indexed_files, total_chunks) = state
            .storage
            .get_index_status(&project_id)
            .await
            .ok()
            .flatten()
            .map(|s| (s.total_files, s.indexed_files, s.total_chunks))
            .unwrap_or((0, 0, 0));
        return Ok(success_json(json!({
            "project_id": project_id,
            "status": "indexing",
            "total_files": total_files,
            "indexed_files": indexed_files,
            "total_chunks": total_chunks,
            "message": "Indexing already in progress"
        })));
    }

    // Spawn indexing in background
    let state_clone = state.clone();
    let path_clone = params.path.clone();
    let project_id_for_cleanup = project_id.clone();

    tokio::spawn(async move {
        let path = std::path::Path::new(&path_clone);
        match crate::codebase::index_project(state_clone.clone(), path).await {
            Ok(status) => {
                tracing::info!(
                    project_id = %status.project_id,
                    files = status.indexed_files,
                    chunks = status.total_chunks,
                    "Indexing completed"
                );
            }
            Err(e) => {
                tracing::error!("Indexing failed: {}", e);
            }
        }
        // Release the atomic lock regardless of outcome
        if let Ok(mut guard) = state_clone.indexing_projects.lock() {
            guard.remove(&project_id_for_cleanup);
        }
    });

    // Return immediately
    Ok(success_json(json!({
        "project_id": project_id,
        "status": "indexing",
        "message": "Indexing started in background. Use get_index_status to check progress."
    })))
}

pub async fn get_index_status(
    state: &Arc<AppState>,
    params: GetIndexStatusParams,
) -> anyhow::Result<CallToolResult> {
    match state.storage.get_index_status(&params.project_id).await {
        Ok(Some(mut status)) => {
            let mut current_file: Option<String> = None;

            // Always try to fetch current_file from monitor if available, even if failed or stuck
            if let Some(monitor) = state.progress.get(&params.project_id).await {
                if let Ok(cf) = monitor.current_file.read() {
                    if !cf.is_empty() {
                        current_file = Some(cf.clone());
                    }
                }

                // If we are actively indexing, update the progress counters
                if status.status == crate::types::IndexState::Indexing {
                    let indexed = monitor
                        .indexed_files
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let total = monitor
                        .total_files
                        .load(std::sync::atomic::Ordering::Relaxed);

                    if indexed > 0 {
                        status.indexed_files = std::cmp::max(status.indexed_files, indexed);
                    }
                    if total > 0 {
                        status.total_files = std::cmp::max(status.total_files, total);
                    }
                }
            }

            // Use manifest entry count as the authoritative "indexed files" number.
            let indexed_files = state
                .storage
                .count_manifest_entries(&params.project_id)
                .await
                .unwrap_or(0) as u32;

            // Sync queue status from shared AtomicUsize counter.
            let sync_queue_size = {
                let map = state.index_pending.read().await;
                map.get(&params.project_id)
                    .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(0)
            };
            let is_syncing = sync_queue_size > 0;

            let total_symbols = state
                .storage
                .count_symbols(&params.project_id)
                .await
                .unwrap_or(0);
            let total_chunks = state
                .storage
                .count_chunks(&params.project_id)
                .await
                .unwrap_or(0);
            let embedded_symbols = state
                .storage
                .count_embedded_symbols(&params.project_id)
                .await
                .unwrap_or(0);
            let embedded_chunks = state
                .storage
                .count_embedded_chunks(&params.project_id)
                .await
                .unwrap_or(0);

            let vector_progress = if total_chunks > 0 {
                (embedded_chunks as f32 / total_chunks as f32) * 100.0
            } else {
                0.0
            };
            let graph_progress = if total_symbols > 0 {
                (embedded_symbols as f32 / total_symbols as f32) * 100.0
            } else {
                0.0
            };

            // Two-phase progress: parsing (70%) + embedding (30%).
            //
            // During parsing, total_chunks/total_symbols grow dynamically as
            // the parser produces output.  The embedding worker typically keeps
            // pace, so  embedded/total ≈ 1.0  —  which made the OLD formula
            // report ~98% when only a fraction of files had been parsed.
            //
            // Fix: anchor progress to the file-parsing ratio (known up-front)
            // and let embedding fill a smaller tail portion.
            //
            //   overall = parse_ratio × (PARSE_W + embed_ratio × EMBED_W) × 100
            //
            // Properties:
            //   • Monotonically increasing (parse_ratio only grows; embed_ratio ≈ 1)
            //   • Continuous at the phase boundary (parse_ratio = 1 ⇒ formula
            //     becomes  (0.70 + embed_ratio × 0.30) × 100 )
            //   • Accurate: 50% files parsed + embedder keeping up  →  ~50%
            const PARSE_WEIGHT: f32 = 0.70;
            const EMBED_WEIGHT: f32 = 0.30;

            let parse_ratio = if status.total_files > 0 {
                (indexed_files as f32 / status.total_files as f32).min(1.0)
            } else {
                0.0
            };

            let embed_ratio = if (total_chunks + total_symbols) > 0 {
                (embedded_chunks + embedded_symbols) as f32 / (total_chunks + total_symbols) as f32
            } else {
                // Nothing to embed (yet or ever) — treat as fully caught up
                1.0
            };

            let overall_progress =
                parse_ratio * (PARSE_WEIGHT + embed_ratio * EMBED_WEIGHT) * 100.0;

            let parsing_done = status.total_files > 0 && indexed_files >= status.total_files;

            Ok(success_json(json!({
                "project_id": status.project_id,
                "status": status.status.to_string(),
                "is_syncing": is_syncing,
                "sync_queue_size": sync_queue_size,
                "total_files": status.total_files,
                "indexed_files": indexed_files,
                "started_at": status.started_at,
                "completed_at": status.completed_at,

                "parsing": {
                    "status": if indexed_files >= status.total_files { "completed" } else { "in_progress" },
                    "progress": format!("{}/{}", indexed_files, status.total_files),
                    "current_file": current_file
                },

                "vector_embeddings": {
                    "status": if status.status == crate::types::IndexState::Completed
                        || (parsing_done && embedded_chunks >= total_chunks && total_chunks > 0)
                        { "completed" } else { "in_progress" },
                    "total": total_chunks,
                    "completed": embedded_chunks,
                    "percent": format!("{:.1}", vector_progress)
                },

                "graph_embeddings": {
                    "status": if status.status == crate::types::IndexState::Completed
                        || (parsing_done && embedded_symbols >= total_symbols && total_symbols > 0)
                        { "completed" } else { "in_progress" },
                    "total": total_symbols,
                    "completed": embedded_symbols,
                    "percent": format!("{:.1}", graph_progress)
                },

                "overall_progress": {
                    "percent": format!("{:.1}", overall_progress),
                    "is_complete": status.status == crate::types::IndexState::Completed
                        || (parsing_done
                            && embedded_chunks >= total_chunks
                            && embedded_symbols >= total_symbols
                            && total_chunks > 0)
                },
                "error_message": status.error_message,
                "failed_files": status.failed_files
            })))
        }
        Ok(None) => Ok(error_response(format!(
            "Project not found: {}",
            params.project_id
        ))),
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn list_projects(
    state: &Arc<AppState>,
    _params: ListProjectsParams,
) -> anyhow::Result<CallToolResult> {
    match state.storage.list_projects().await {
        Ok(projects) => {
            let mut enriched = Vec::with_capacity(projects.len());

            for project_id in &projects {
                let status = state
                    .storage
                    .get_index_status(project_id)
                    .await
                    .ok()
                    .flatten();
                let chunks = state.storage.count_chunks(project_id).await.unwrap_or(0);
                let symbols = state.storage.count_symbols(project_id).await.unwrap_or(0);
                let embedded_chunks = state
                    .storage
                    .count_embedded_chunks(project_id)
                    .await
                    .unwrap_or(0);
                let embedded_symbols = state
                    .storage
                    .count_embedded_symbols(project_id)
                    .await
                    .unwrap_or(0);

                let status_str = status
                    .as_ref()
                    .map(|s| s.status.to_string())
                    .unwrap_or_else(|| "unknown".to_string());

                enriched.push(json!({
                    "id": project_id,
                    "status": status_str,
                    "chunks": chunks,
                    "symbols": symbols,
                    "embedded_chunks": embedded_chunks,
                    "embedded_symbols": embedded_symbols
                }));
            }

            Ok(success_json(json!({
                "projects": enriched,
                "count": projects.len()
            })))
        }
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn delete_project(
    state: &Arc<AppState>,
    params: DeleteProjectParams,
) -> anyhow::Result<CallToolResult> {
    let _ = state
        .storage
        .delete_project_symbols(&params.project_id)
        .await;

    let _ = state.storage.delete_index_status(&params.project_id).await;
    let _ = state.storage.delete_manifest_entries(&params.project_id).await;

    // Remove from in-memory BM25 index
    state.code_search.remove_project(&params.project_id).await;

    match state
        .storage
        .delete_project_chunks(&params.project_id)
        .await
    {
        Ok(deleted) => Ok(success_json(json!({
            "deleted_chunks": deleted,
            "project_id": params.project_id
        }))),
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn get_project_stats(
    state: &Arc<AppState>,
    params: GetProjectStatsParams,
) -> anyhow::Result<CallToolResult> {
    let status = state.storage.get_index_status(&params.project_id).await?;

    if status.is_none() {
        return Ok(error_response(format!(
            "Project not found: {}",
            params.project_id
        )));
    }

    let status = status.unwrap();

    let total_symbols = state
        .storage
        .count_symbols(&params.project_id)
        .await
        .unwrap_or(0);
    let total_chunks = state
        .storage
        .count_chunks(&params.project_id)
        .await
        .unwrap_or(0);
    let embedded_symbols = state
        .storage
        .count_embedded_symbols(&params.project_id)
        .await
        .unwrap_or(0);
    let embedded_chunks = state
        .storage
        .count_embedded_chunks(&params.project_id)
        .await
        .unwrap_or(0);

    // Use manifest entry count as the authoritative "indexed files" number.
    let indexed_files = state
        .storage
        .count_manifest_entries(&params.project_id)
        .await
        .unwrap_or(0) as u32;

    // Sync queue status from shared AtomicUsize counter.
    let sync_queue_size = {
        let map = state.index_pending.read().await;
        map.get(&params.project_id)
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0)
    };
    let is_syncing = sync_queue_size > 0;

    let vector_progress = if total_chunks > 0 {
        (embedded_chunks as f32 / total_chunks as f32) * 100.0
    } else {
        0.0
    };
    let graph_progress = if total_symbols > 0 {
        (embedded_symbols as f32 / total_symbols as f32) * 100.0
    } else {
        0.0
    };

    // Two-phase overall progress (same formula as get_index_status)
    const PARSE_WEIGHT: f32 = 0.70;
    const EMBED_WEIGHT: f32 = 0.30;

    let parse_ratio = if status.total_files > 0 {
        (indexed_files as f32 / status.total_files as f32).min(1.0)
    } else {
        0.0
    };
    let embed_ratio = if (total_chunks + total_symbols) > 0 {
        (embedded_chunks + embedded_symbols) as f32 / (total_chunks + total_symbols) as f32
    } else {
        1.0
    };
    let overall_progress = parse_ratio * (PARSE_WEIGHT + embed_ratio * EMBED_WEIGHT) * 100.0;

    Ok(success_json(json!({
        "project_id": params.project_id,
        "status": status.status.to_string(),
        "is_syncing": is_syncing,
        "sync_queue_size": sync_queue_size,
        "files": {
            "total": status.total_files,
            "indexed": indexed_files,
            "parse_percent": format!("{:.1}", parse_ratio * 100.0)
        },
        "chunks": {
            "total": total_chunks,
            "embedded": embedded_chunks,
            "progress_percent": format!("{:.1}", vector_progress)
        },
        "symbols": {
            "total": total_symbols,
            "embedded": embedded_symbols,
            "progress_percent": format!("{:.1}", graph_progress)
        },
        "overall_progress_percent": format!("{:.1}", overall_progress),
        "started_at": status.started_at,
        "completed_at": status.completed_at,
        "failed_files": status.failed_files
    })))
}

/// Returns indexing degradation info for any active project, or None if all complete.
/// Used by recall_code, search_symbols, symbol_graph to inform users about degraded features.
pub async fn get_degradation_info(state: &Arc<AppState>) -> Option<serde_json::Value> {
    let project_ids = state.storage.list_projects().await.ok()?;

    for project_id in &project_ids {
        let status = match state.storage.get_index_status(project_id).await {
            Ok(Some(s)) => s,
            _ => continue,
        };

        if status.status == crate::types::IndexState::Completed
            || status.status == crate::types::IndexState::Failed
        {
            continue;
        }

        let total_chunks = state.storage.count_chunks(project_id).await.unwrap_or(0);
        let embedded_chunks = state.storage.count_embedded_chunks(project_id).await.unwrap_or(0);
        let total_symbols = state.storage.count_symbols(project_id).await.unwrap_or(0);
        let embedded_symbols = state.storage.count_embedded_symbols(project_id).await.unwrap_or(0);

        let chunk_pct = if total_chunks > 0 {
            (embedded_chunks as f64 / total_chunks as f64) * 100.0
        } else {
            0.0
        };

        return Some(json!({
            "status": status.status.to_string(),
            "progress": format!("{}/{} chunks ({:.1}%), {}/{} symbols",
                embedded_chunks, total_chunks, chunk_pct, embedded_symbols, total_symbols),
            "degraded": ["vector_search"],
            "available": ["bm25_search", "symbol_search", "ppr_graph"],
            "message": "Indexing in progress. Semantic (vector) search unavailable until complete."
        }));
    }

    None
}
