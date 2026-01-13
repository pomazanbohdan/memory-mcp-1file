use std::sync::Arc;

use rmcp::model::CallToolResult;
use serde_json::json;

use crate::config::{AppState, STATUS_ERROR, STATUS_INITIALIZING, STATUS_READY};
use crate::embedding::EmbeddingStatus;
use crate::server::params::{GetStatusParams, ResetAllMemoryParams};
use crate::storage::StorageBackend;

use super::{error_response, success_json};

pub async fn get_status(
    state: &Arc<AppState>,
    _params: GetStatusParams,
) -> anyhow::Result<CallToolResult> {
    let storage_status = match state.status_code() {
        STATUS_INITIALIZING => "initializing",
        STATUS_READY => "ready",
        STATUS_ERROR => "error",
        _ => "unknown",
    };

    let (memories_count, db_healthy) = if let Some(storage) = state.storage() {
        (
            storage.count_memories().await.unwrap_or(0),
            storage.health_check().await.unwrap_or(false),
        )
    } else {
        (0, false)
    };

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

    let status = if storage_status == "initializing" {
        "initializing"
    } else if storage_status == "error" {
        "error"
    } else if !db_healthy {
        "degraded"
    } else {
        overall_status
    };

    let init_error = state.init_error.read().await;

    Ok(success_json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "status": status,
        "storage_status": storage_status,
        "memories_count": memories_count,
        "embedding": embedding_json,
        "init_error": *init_error
    })))
}

pub async fn reset_all_memory(
    state: &Arc<AppState>,
    params: ResetAllMemoryParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    if !params.confirm {
        return Ok(error_response("Must set confirm=true to reset all data"));
    }

    let storage = state.storage().unwrap();
    storage.reset_db().await?;

    Ok(success_json(json!({
        "reset": true,
        "warning": "All data has been cleared"
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TestContext;
    use crate::types::{Memory, MemoryType};

    #[tokio::test]
    async fn test_system_logic() {
        let ctx = TestContext::new().await;

        let storage = ctx
            .state
            .storage()
            .expect("Storage should be ready in tests");
        storage
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

        let status_params = GetStatusParams {
            _placeholder: false,
        };
        let status_res = get_status(&ctx.state, status_params).await.unwrap();
        let status_val = serde_json::to_value(&status_res).unwrap();
        let status_text = status_val["content"][0]["text"].as_str().unwrap();
        let status_json: serde_json::Value = serde_json::from_str(status_text).unwrap();
        assert_eq!(status_json["memories_count"].as_u64().unwrap(), 1);

        let reset_params_fail = ResetAllMemoryParams { confirm: false };
        let reset_res_fail = reset_all_memory(&ctx.state, reset_params_fail)
            .await
            .unwrap();
        let fail_val = serde_json::to_value(&reset_res_fail).unwrap();
        let fail_text = fail_val["content"][0]["text"].as_str().unwrap();
        let fail_json: serde_json::Value = serde_json::from_str(fail_text).unwrap();
        assert!(fail_json.get("error").is_some());

        let reset_params = ResetAllMemoryParams { confirm: true };
        let reset_res = reset_all_memory(&ctx.state, reset_params).await.unwrap();
        let success_val = serde_json::to_value(&reset_res).unwrap();
        let success_text = success_val["content"][0]["text"].as_str().unwrap();
        let success_json: serde_json::Value = serde_json::from_str(success_text).unwrap();
        assert!(success_json.get("reset").is_some());

        assert_eq!(storage.count_memories().await.unwrap(), 0);
    }
}
