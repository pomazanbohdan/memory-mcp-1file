use std::sync::Arc;

use rmcp::model::CallToolResult;
use serde_json::json;

use crate::config::AppState;
use crate::embedding::ContentHasher;
use crate::server::params::{
    DeleteMemoryParams, GetMemoryParams, GetValidAtParams, GetValidParams, InvalidateParams,
    ListMemoriesParams, StoreMemoryParams, UpdateMemoryParams,
};
use crate::storage::StorageBackend;
use crate::types::EmbeddingState;
use crate::types::{Memory, MemoryType, MemoryUpdate};

use super::{error_response, normalize_limit, strip_embedding, strip_embeddings, success_json};

pub async fn store_memory(
    state: &Arc<AppState>,
    params: StoreMemoryParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);
    crate::ensure_embedding_ready!(state);

    let storage = state.storage().unwrap();
    let embedding = state.embedding.embed(&params.content).await?;

    let mem_type: MemoryType = params
        .memory_type
        .as_ref()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    let now = surrealdb::sql::Datetime::default();
    let memory = Memory {
        content: params.content,
        embedding: Some(embedding),
        memory_type: mem_type,
        user_id: params.user_id,
        metadata: params.metadata,
        event_time: now.clone(),
        ingestion_time: now.clone(),
        valid_from: now,
        ..Default::default()
    };

    match storage.create_memory(memory).await {
        Ok(id) => Ok(success_json(json!({ "id": id }))),
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn get_memory(
    state: &Arc<AppState>,
    params: GetMemoryParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    match storage.get_memory(&params.id).await {
        Ok(Some(mut memory)) => {
            strip_embedding(&mut memory);
            Ok(success_json(
                serde_json::to_value(&memory).unwrap_or_default(),
            ))
        }
        Ok(None) => Ok(error_response(format!("Memory not found: {}", params.id))),
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn update_memory(
    state: &Arc<AppState>,
    params: UpdateMemoryParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    let (embedding, content_hash, embedding_state) = if let Some(ref new_content) = params.content {
        let old_memory = storage.get_memory(&params.id).await?;
        let old_hash = old_memory.as_ref().and_then(|m| m.content_hash.as_deref());

        if ContentHasher::needs_reembed(old_hash, new_content) {
            let emb = state.embedding.embed(new_content).await?;
            let hash = ContentHasher::hash(new_content);
            (Some(emb), Some(hash), Some(EmbeddingState::Ready))
        } else {
            (None, None, None)
        }
    } else {
        (None, None, None)
    };

    let update = MemoryUpdate {
        content: params.content,
        memory_type: params.memory_type.and_then(|s| s.parse().ok()),
        metadata: params.metadata,
        embedding,
        content_hash,
        embedding_state,
    };

    match storage.update_memory(&params.id, update).await {
        Ok(mut memory) => {
            strip_embedding(&mut memory);
            Ok(success_json(
                serde_json::to_value(&memory).unwrap_or_default(),
            ))
        }
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn delete_memory(
    state: &Arc<AppState>,
    params: DeleteMemoryParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    match storage.delete_memory(&params.id).await {
        Ok(deleted) => Ok(success_json(json!({ "deleted": deleted }))),
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn list_memories(
    state: &Arc<AppState>,
    params: ListMemoriesParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    let limit = normalize_limit(params.limit);
    let offset = params.offset.unwrap_or(0);

    let mut memories = match storage.list_memories(limit, offset).await {
        Ok(m) => m,
        Err(e) => return Ok(error_response(e)),
    };

    strip_embeddings(&mut memories);
    let total = storage.count_memories().await.unwrap_or(0);

    Ok(success_json(json!({
        "memories": memories,
        "total": total,
        "limit": limit,
        "offset": offset
    })))
}

pub async fn get_valid(
    state: &Arc<AppState>,
    params: GetValidParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    let limit = normalize_limit(params.limit);

    match storage.get_valid(params.user_id.as_deref(), limit).await {
        Ok(mut memories) => {
            strip_embeddings(&mut memories);
            Ok(success_json(json!({
                "memories": memories,
                "count": memories.len()
            })))
        }
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn get_valid_at(
    state: &Arc<AppState>,
    params: GetValidAtParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    let limit = normalize_limit(params.limit);

    let chrono_ts: chrono::DateTime<chrono::Utc> = match params.timestamp.parse() {
        Ok(t) => t,
        Err(_) => return Ok(error_response("Invalid timestamp format. Use ISO 8601")),
    };
    let ts = surrealdb::sql::Datetime::from(chrono_ts);

    match storage
        .get_valid_at(ts, params.user_id.as_deref(), limit)
        .await
    {
        Ok(mut memories) => {
            strip_embeddings(&mut memories);
            Ok(success_json(json!({
                "memories": memories,
                "count": memories.len(),
                "timestamp": params.timestamp
            })))
        }
        Err(e) => Ok(error_response(e)),
    }
}

pub async fn invalidate(
    state: &Arc<AppState>,
    params: InvalidateParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    match storage
        .invalidate(
            &params.id,
            params.reason.as_deref(),
            params.superseded_by.as_deref(),
        )
        .await
    {
        Ok(success) => Ok(success_json(json!({ "invalidated": success }))),
        Err(e) => Ok(error_response(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TestContext;

    #[tokio::test]
    async fn test_memory_crud_logic() {
        let ctx = TestContext::new().await;

        // 1. Store
        let params = StoreMemoryParams {
            content: "Logic test memory".to_string(),
            memory_type: Some("semantic".to_string()),
            user_id: Some("user1".to_string()),
            metadata: None,
        };
        let result = store_memory(&ctx.state, params).await.unwrap();
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();
        let id = json["id"].as_str().unwrap().to_string();

        // 2. Get
        let get_params = GetMemoryParams { id: id.clone() };
        let result = get_memory(&ctx.state, get_params).await.unwrap();
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let memory_json: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(memory_json["content"], "Logic test memory");

        // 3. List
        let list_params = ListMemoriesParams {
            limit: Some(10),
            offset: None,
        };
        let result = list_memories(&ctx.state, list_params).await.unwrap();
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let list_json: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(list_json["memories"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_ensure_storage_ready_when_ready() {
        let ctx = TestContext::new().await;

        let params = ListMemoriesParams {
            limit: Some(10),
            offset: None,
        };
        let result = list_memories(&ctx.state, params).await.unwrap();
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();

        assert!(json.get("memories").is_some());
        assert!(json.get("status").is_none() || json["status"] != "initializing");
    }

    #[tokio::test]
    async fn test_store_memory_requires_storage_ready() {
        let ctx = TestContext::new().await;

        let params = StoreMemoryParams {
            content: "Test lazy init".to_string(),
            memory_type: None,
            user_id: None,
            metadata: None,
        };
        let result = store_memory(&ctx.state, params).await.unwrap();
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();

        assert!(json.get("id").is_some());
    }
}
