use std::sync::Arc;

use rmcp::model::CallToolResult;
use serde_json::json;

use crate::config::AppState;
use crate::server::params::{
    DeleteProjectParams, GetCalleesParams, GetCallersParams, GetIndexStatusParams,
    GetProjectStatsParams, IndexProjectParams, ListProjectsParams, SearchCodeParams,
    SearchSymbolsParams,
};
use crate::storage::StorageBackend;

use super::{error_response, normalize_limit, strip_symbol_embeddings, success_json};

pub async fn index_project(
    state: &Arc<AppState>,
    params: IndexProjectParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
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

    if let Ok(Some(status)) = storage.get_index_status(&project_id).await {
        match status.status {
            crate::types::IndexState::Indexing => {
                return Ok(success_json(json!({
                    "project_id": project_id,
                    "status": "indexing",
                    "total_files": status.total_files,
                    "indexed_files": status.indexed_files,
                    "total_chunks": status.total_chunks,
                    "message": "Indexing already in progress"
                })));
            }
            crate::types::IndexState::Completed => {
                tracing::info!(project_id = %project_id, "Re-indexing project (was completed)");
            }
            _ => {}
        }
    }

    // Spawn indexing in background
    let state_clone = state.clone();
    let path_clone = params.path.clone();

    tokio::spawn(async move {
        let path = std::path::Path::new(&path_clone);
        match crate::codebase::index_project(state_clone, path).await {
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
    });

    // Return immediately
    Ok(success_json(json!({
        "project_id": project_id,
        "status": "indexing",
        "message": "Indexing started in background. Use get_index_status to check progress."
    })))
}

pub async fn search_code(
    state: &Arc<AppState>,
    params: SearchCodeParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);
    crate::ensure_embedding_ready!(state);

    let storage = state.storage().unwrap();

    if let Some(ref project_id) = params.project_id {
        if let Ok(Some(status)) = storage.get_index_status(project_id).await {
            if status.status == crate::types::IndexState::Indexing {
                return Ok(success_json(json!({
                    "status": "indexing",
                    "project_id": project_id,
                    "progress": format!("{}/{} files", status.indexed_files, status.total_files),
                    "total_chunks": status.total_chunks,
                    "message": "Indexing in progress. Results may be incomplete."
                })));
            }
        }
    }

    let query_embedding = state.embedding.embed(&params.query).await?;

    let limit = normalize_limit(params.limit);
    let results = storage
        .vector_search_code(&query_embedding, params.project_id.as_deref(), limit)
        .await
        .unwrap_or_default();

    if !results.is_empty() {
        return Ok(success_json(json!({
            "results": results,
            "count": results.len(),
            "query": params.query
        })));
    }

    match storage
        .bm25_search_code(&params.query, params.project_id.as_deref(), limit)
        .await
    {
        Ok(fallback) => Ok(success_json(json!({
            "results": fallback,
            "count": fallback.len(),
            "query": params.query,
            "note": "fallback to text search"
        }))),
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn get_index_status(
    state: &Arc<AppState>,
    params: GetIndexStatusParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    match storage.get_index_status(&params.project_id).await {
        Ok(Some(mut status)) => {
            if status.status == crate::types::IndexState::Indexing {
                if let Some(monitor) = state.progress.get(&params.project_id).await {
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

            let total_symbols = storage.count_symbols(&params.project_id).await.unwrap_or(0);
            let total_chunks = storage.count_chunks(&params.project_id).await.unwrap_or(0);
            let embedded_symbols = storage
                .count_embedded_symbols(&params.project_id)
                .await
                .unwrap_or(0);
            let embedded_chunks = storage
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
            let overall_progress = if (total_chunks + total_symbols) > 0 {
                ((embedded_chunks + embedded_symbols) as f32
                    / (total_chunks + total_symbols) as f32)
                    * 100.0
            } else {
                0.0
            };

            Ok(success_json(json!({
                "project_id": status.project_id,
                "status": status.status.to_string(),
                "total_files": status.total_files,
                "indexed_files": status.indexed_files,
                "started_at": status.started_at,
                "completed_at": status.completed_at,

                "parsing": {
                    "status": if status.indexed_files >= status.total_files { "completed" } else { "in_progress" },
                    "progress": format!("{}/{}", status.indexed_files, status.total_files)
                },

                "vector_embeddings": {
                    "status": if embedded_chunks >= total_chunks && total_chunks > 0 { "completed" } else { "in_progress" },
                    "total": total_chunks,
                    "completed": embedded_chunks,
                    "percent": format!("{:.1}", vector_progress)
                },

                "graph_embeddings": {
                    "status": if embedded_symbols >= total_symbols && total_symbols > 0 { "completed" } else { "in_progress" },
                    "total": total_symbols,
                    "completed": embedded_symbols,
                    "percent": format!("{:.1}", graph_progress)
                },

                "overall_progress": {
                    "percent": format!("{:.1}", overall_progress),
                    "is_complete": embedded_chunks >= total_chunks && embedded_symbols >= total_symbols && total_chunks > 0
                }
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
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    match storage.list_projects().await {
        Ok(projects) => {
            let mut enriched = Vec::with_capacity(projects.len());

            for project_id in &projects {
                let status = storage.get_index_status(project_id).await.ok().flatten();
                let chunks = storage.count_chunks(project_id).await.unwrap_or(0);
                let symbols = storage.count_symbols(project_id).await.unwrap_or(0);
                let embedded_chunks = storage.count_embedded_chunks(project_id).await.unwrap_or(0);
                let embedded_symbols = storage
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
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    let _ = storage.delete_project_symbols(&params.project_id).await;

    let _ = storage.delete_index_status(&params.project_id).await;

    match storage.delete_project_chunks(&params.project_id).await {
        Ok(deleted) => Ok(success_json(json!({
            "deleted_chunks": deleted,
            "project_id": params.project_id
        }))),
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn search_symbols(
    state: &Arc<AppState>,
    params: SearchSymbolsParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let offset = params.offset.unwrap_or(0);

    match storage
        .search_symbols(
            &params.query,
            params.project_id.as_deref(),
            limit,
            offset,
            params.symbol_type.as_deref(),
            params.path_prefix.as_deref(),
        )
        .await
    {
        Ok((mut symbols, total)) => {
            let count = symbols.len();
            strip_symbol_embeddings(&mut symbols);

            let has_more = offset + count < total as usize;

            Ok(success_json(json!({
                "results": symbols,
                "count": count,
                "total": total,
                "offset": offset,
                "limit": limit,
                "has_more": has_more,
                "query": params.query,
                "filters": {
                    "project_id": params.project_id,
                    "symbol_type": params.symbol_type,
                    "path_prefix": params.path_prefix
                }
            })))
        }
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn get_callers(
    state: &Arc<AppState>,
    params: GetCallersParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    match storage.get_symbol_callers(&params.symbol_id).await {
        Ok(mut callers) => {
            strip_symbol_embeddings(&mut callers);
            Ok(success_json(json!({
                "results": callers,
                "count": callers.len(),
                "symbol_id": params.symbol_id
            })))
        }
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn get_callees(
    state: &Arc<AppState>,
    params: GetCalleesParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    match storage.get_symbol_callees(&params.symbol_id).await {
        Ok(mut callees) => {
            strip_symbol_embeddings(&mut callees);
            Ok(success_json(json!({
                "results": callees,
                "count": callees.len(),
                "symbol_id": params.symbol_id
            })))
        }
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn get_related_symbols(
    state: &Arc<AppState>,
    params: crate::server::params::GetRelatedSymbolsParams,
) -> anyhow::Result<CallToolResult> {
    use crate::types::Direction;

    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    let depth = params.depth.unwrap_or(1).min(3);
    let direction: Direction = params
        .direction
        .as_ref()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    match storage
        .get_related_symbols(&params.symbol_id, depth, direction)
        .await
    {
        Ok((mut symbols, relations)) => {
            strip_symbol_embeddings(&mut symbols);
            Ok(success_json(json!({
                "symbols": symbols,
                "relations": relations,
                "symbol_count": symbols.len(),
                "relation_count": relations.len()
            })))
        }
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn get_project_stats(
    state: &Arc<AppState>,
    params: GetProjectStatsParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    let status = storage.get_index_status(&params.project_id).await?;

    if status.is_none() {
        return Ok(error_response(format!(
            "Project not found: {}",
            params.project_id
        )));
    }

    let status = status.unwrap();

    let total_symbols = storage.count_symbols(&params.project_id).await.unwrap_or(0);
    let total_chunks = storage.count_chunks(&params.project_id).await.unwrap_or(0);
    let embedded_symbols = storage
        .count_embedded_symbols(&params.project_id)
        .await
        .unwrap_or(0);
    let embedded_chunks = storage
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

    Ok(success_json(json!({
        "project_id": params.project_id,
        "status": status.status.to_string(),
        "files": {
            "total": status.total_files,
            "indexed": status.indexed_files
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
        "started_at": status.started_at,
        "completed_at": status.completed_at
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TestContext;
    use std::fs;

    #[tokio::test]
    async fn test_code_logic_flow() {
        let ctx = TestContext::new().await;
        let unique_id = format!("test_project_{}", surrealdb::Uuid::new_v4().simple());
        let project_path = ctx._temp_dir.path().join(&unique_id);
        fs::create_dir_all(&project_path).unwrap();
        fs::write(
            project_path.join("main.rs"),
            "fn main() { println!(\"Hello\"); }",
        )
        .unwrap();

        let index_params = IndexProjectParams {
            path: project_path.to_string_lossy().to_string(),
        };

        // 1. Trigger Indexing
        let result = index_project(&ctx.state, index_params).await.unwrap();
        // Should return "indexing" status immediately
        if let rmcp::model::RawContent::Text(t) = &result.content[0].raw {
            assert!(t.text.contains("indexing"));
        } else {
            panic!("Expected text content");
        }

        // 2. Wait for indexing to complete
        // Since it's a background task, we poll get_index_status
        let status_params = GetIndexStatusParams {
            project_id: unique_id.clone(),
        };

        let mut retries = 0;
        let mut last_status = String::new();
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            let res = get_index_status(&ctx.state, status_params.clone())
                .await
                .unwrap();
            if let rmcp::model::RawContent::Text(t) = &res.content[0].raw {
                last_status = t.text.clone();
                let wait_for_full_completion = t.text.contains("\"status\":\"completed\"");
                if wait_for_full_completion {
                    break;
                }
            }
            retries += 1;
            if retries > 100 {
                panic!("Indexing timed out. Last status: {}", last_status);
            }
        }

        // 3. Search Code
        let search_params = SearchCodeParams {
            query: "Hello".to_string(),
            project_id: Some(unique_id.clone()),
            limit: Some(5),
        };
        let search_res = search_code(&ctx.state, search_params).await.unwrap();

        if let rmcp::model::RawContent::Text(t) = &search_res.content[0].raw {
            assert!(
                t.text.contains("main.rs"),
                "Expected 'main.rs' in search results. Got: {}",
                &t.text[..std::cmp::min(500, t.text.len())]
            );
            assert!(t.text.contains("Hello"));
        } else {
            panic!("Expected text content");
        }
    }
}
