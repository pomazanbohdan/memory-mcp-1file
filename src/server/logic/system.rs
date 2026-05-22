use std::sync::Arc;

use rmcp::model::CallToolResult;
use serde_json::json;

use crate::config::AppState;
use crate::embedding::EmbeddingStatus;
use crate::server::params::{GetStatusParams, ResetAllMemoryParams};
use crate::storage::StorageBackend;

use super::{error_response, success_json};

pub async fn get_status(
    state: &Arc<AppState>,
    _params: GetStatusParams,
) -> anyhow::Result<CallToolResult> {
    let memories_count = state.storage.count_memories().await.unwrap_or(0);
    let db_healthy = state.storage.health_check().await.unwrap_or(false);
    let embedding_status = state.embedding.status().await;

    let (overall_status, embedding_json) = match &embedding_status {
        EmbeddingStatus::Ready => (
            "healthy",
            json!({
                "status": "ready",
                "model": format!("{}_{}", state.embedding.model(), state.embedding.dimensions()),
                "dimensions": state.embedding.dimensions()
            }),
        ),
        EmbeddingStatus::Loading {
            phase,
            elapsed_seconds,
            eta_seconds,
            cached,
            progress_percent,
            ..
        } => {
            let mut loading_json = json!({
                "status": "loading",
                "phase": phase.to_string(),
                "elapsed_seconds": elapsed_seconds,
                "eta_seconds": eta_seconds,
                "cached": cached,
                "model": format!("{}_{}", state.embedding.model(), state.embedding.dimensions()),
                "dimensions": state.embedding.dimensions()
            });
            if let Some(pct) = progress_percent {
                loading_json["progress_percent"] = json!(pct);
            }
            ("loading", loading_json)
        }
        EmbeddingStatus::Error { message } => (
            "error",
            json!({
                "status": "error",
                "error": message,
                "model": format!("{}_{}", state.embedding.model(), state.embedding.dimensions()),
                "dimensions": state.embedding.dimensions()
            }),
        ),
    };

    let status = if !db_healthy {
        "degraded"
    } else {
        overall_status
    };

    Ok(success_json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "status": status,
        "memories_count": memories_count,
        "embedding": embedding_json
    })))
}

pub async fn reset_all_memory(
    state: &Arc<AppState>,
    params: ResetAllMemoryParams,
) -> anyhow::Result<CallToolResult> {
    if !params.confirm {
        return Ok(error_response("Must set confirm=true to reset all data"));
    }

    state.storage.reset_db().await?;
    state.code_search.clear_all().await;

    Ok(success_json(json!({
        "reset": true,
        "warning": "All data has been cleared"
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::ChunkMeta;
    use crate::test_utils::TestContext;
    use crate::types::{ChunkType, Language, Memory, MemoryType};

    #[tokio::test]
    async fn test_system_logic() {
        let ctx = TestContext::new().await;

        // Seed
        ctx.state
            .storage
            .create_memory(Memory {
                id: None,
                content: "To be reset".to_string(),
                embedding: None,
                memory_type: MemoryType::Semantic,
                user_id: None,
                metadata: None,
                event_time: Default::default(),
                ingestion_time: Default::default(),
                valid_from: Default::default(),
                valid_until: None,
                importance_score: 1.0,
                invalidation_reason: None,
                content_hash: None,
                embedding_state: Default::default(),
            })
            .await
            .unwrap();
        ctx.state
            .code_search
            .upsert_chunks(
                "proj-reset",
                vec![(
                    ChunkMeta {
                        id: "chunk-1".to_string(),
                        file_path: "src/private.rs".to_string(),
                        language: Language::Rust,
                        start_line: 1,
                        end_line: 5,
                        chunk_type: ChunkType::Function,
                        name: Some("secret_fn".to_string()),
                        context_path: None,
                        project_id: "proj-reset".to_string(),
                    },
                    "secret token".to_string(),
                )],
            )
            .await;
        assert_eq!(ctx.state.code_search.chunk_count("proj-reset").await, 1);

        // 1. Get Status
        let status_params = GetStatusParams {
            _placeholder: false,
        };
        let status_res = get_status(&ctx.state, status_params).await.unwrap();
        let status_val = serde_json::to_value(&status_res).unwrap();
        let status_text = status_val["content"][0]["text"].as_str().unwrap();
        let status_json: serde_json::Value = serde_json::from_str(status_text).unwrap();
        assert_eq!(status_json["memories_count"].as_u64().unwrap(), 1);

        // 2. Reset without confirm
        let reset_params_fail = ResetAllMemoryParams { confirm: false };
        let reset_res_fail = reset_all_memory(&ctx.state, reset_params_fail)
            .await
            .unwrap();
        let fail_val = serde_json::to_value(&reset_res_fail).unwrap();
        let fail_text = fail_val["content"][0]["text"].as_str().unwrap();
        let fail_json: serde_json::Value = serde_json::from_str(fail_text).unwrap();
        assert!(fail_json.get("error").is_some());

        // 3. Reset with confirm
        let reset_params = ResetAllMemoryParams { confirm: true };
        let reset_res = reset_all_memory(&ctx.state, reset_params).await.unwrap();
        let success_val = serde_json::to_value(&reset_res).unwrap();
        let success_text = success_val["content"][0]["text"].as_str().unwrap();
        let success_json: serde_json::Value = serde_json::from_str(success_text).unwrap();
        assert!(success_json.get("reset").is_some());

        assert_eq!(ctx.state.storage.count_memories().await.unwrap(), 0);
        assert_eq!(ctx.state.code_search.chunk_count("proj-reset").await, 0);
    }
}
