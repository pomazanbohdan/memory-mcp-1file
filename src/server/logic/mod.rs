pub mod code;
pub mod graph;
pub mod memory;
pub mod search;
pub mod system;

use rmcp::model::{CallToolResult, Content};
use serde_json::json;

#[allow(unused_imports)]
use crate::config::{STATUS_ERROR, STATUS_INITIALIZING};
use crate::embedding::EmbeddingStatus;
use crate::types::{CodeSymbol, Entity, Memory};

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;

pub fn normalize_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
}

pub fn error_response(e: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        json!({ "error": e.to_string() }).to_string(),
    )])
}

pub fn storage_initializing_response() -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        json!({
            "status": "initializing",
            "message": "Server initializing, please retry in 1-2 seconds",
            "retry_after_seconds": 2
        })
        .to_string(),
    )])
}

pub fn storage_error_response(error: &Option<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        json!({
            "status": "error",
            "error": error.as_deref().unwrap_or("Storage initialization failed")
        })
        .to_string(),
    )])
}

/// Create success response from JSON value
pub fn success_json(value: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(value.to_string())])
}

/// Create success response from serializable value
pub fn success_serialize<T: serde::Serialize>(value: &T) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        serde_json::to_string(value).unwrap_or_default(),
    )])
}

pub fn embedding_loading_response(status: &EmbeddingStatus) -> CallToolResult {
    match status {
        EmbeddingStatus::Loading {
            phase,
            elapsed_seconds,
            eta_seconds,
            cached,
            progress_percent,
            downloaded_mb,
            total_mb,
        } => {
            let mut response = json!({
                "status": "loading",
                "message": format!("Model loading: {}", phase),
                "phase": phase,
                "elapsed_seconds": elapsed_seconds,
                "eta_seconds": eta_seconds,
                "cached": cached,
                "retry_after_seconds": eta_seconds.unwrap_or(5).min(10)
            });

            if let Some(pct) = progress_percent {
                response["progress_percent"] = json!(pct);
            }
            if let (Some(dl), Some(total)) = (downloaded_mb, total_mb) {
                response["downloaded_mb"] = json!(dl);
                response["total_mb"] = json!(total);
            }

            CallToolResult::success(vec![Content::text(response.to_string())])
        }
        EmbeddingStatus::Error { message } => CallToolResult::success(vec![Content::text(
            json!({
                "status": "error",
                "error": message
            })
            .to_string(),
        )]),
        EmbeddingStatus::Ready => {
            CallToolResult::success(vec![Content::text(json!({"status": "ready"}).to_string())])
        }
    }
}

#[macro_export]
macro_rules! ensure_embedding_ready {
    ($state:expr) => {
        let status = $state.embedding.status().await;
        if !status.is_ready() {
            return Ok($crate::server::logic::embedding_loading_response(&status));
        }
    };
}

#[macro_export]
macro_rules! ensure_storage_ready {
    ($state:expr) => {
        match $state.status_code() {
            $crate::config::STATUS_INITIALIZING => {
                return Ok($crate::server::logic::storage_initializing_response());
            }
            $crate::config::STATUS_ERROR => {
                let error = $state.init_error.read().await;
                return Ok($crate::server::logic::storage_error_response(&error));
            }
            _ => {}
        }
    };
}

pub fn strip_embeddings(memories: &mut [Memory]) {
    for m in memories {
        m.embedding.take();
    }
}

pub fn strip_embedding(memory: &mut Memory) {
    memory.embedding.take();
}

pub fn strip_entity_embeddings(entities: &mut [Entity]) {
    for e in entities {
        e.embedding.take();
    }
}

pub fn strip_symbol_embeddings(symbols: &mut [CodeSymbol]) {
    for s in symbols.iter_mut() {
        s.embedding = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_limit() {
        assert_eq!(normalize_limit(None), DEFAULT_LIMIT);
        assert_eq!(normalize_limit(Some(10)), 10);
        assert_eq!(normalize_limit(Some(50)), 50);
        assert_eq!(normalize_limit(Some(100)), 100);
        assert_eq!(normalize_limit(Some(101)), MAX_LIMIT);
        assert_eq!(normalize_limit(Some(1000)), MAX_LIMIT);
    }

    #[test]
    fn test_storage_initializing_response() {
        let result = storage_initializing_response();
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();

        assert_eq!(json["status"], "initializing");
        assert!(json["message"].as_str().unwrap().contains("initializing"));
        assert_eq!(json["retry_after_seconds"], 2);
    }

    #[test]
    fn test_storage_error_response_with_message() {
        let error = Some("Database connection failed".to_string());
        let result = storage_error_response(&error);
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();

        assert_eq!(json["status"], "error");
        assert_eq!(json["error"], "Database connection failed");
    }

    #[test]
    fn test_storage_error_response_without_message() {
        let error: Option<String> = None;
        let result = storage_error_response(&error);
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();

        assert_eq!(json["status"], "error");
        assert_eq!(json["error"], "Storage initialization failed");
    }

    #[test]
    fn test_embedding_loading_response_loading() {
        use crate::embedding::LoadingPhase;

        let status = EmbeddingStatus::Loading {
            phase: LoadingPhase::FetchingWeights,
            elapsed_seconds: 5,
            eta_seconds: Some(10),
            cached: false,
            progress_percent: Some(50.0),
            downloaded_mb: Some(100.0),
            total_mb: Some(200.0),
        };
        let result = embedding_loading_response(&status);
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();

        assert_eq!(json["status"], "loading");
        assert_eq!(json["elapsed_seconds"], 5);
        assert_eq!(json["eta_seconds"], 10);
        assert_eq!(json["cached"], false);
        assert_eq!(json["progress_percent"], 50.0);
        assert_eq!(json["downloaded_mb"], 100.0);
        assert_eq!(json["total_mb"], 200.0);
    }

    #[test]
    fn test_embedding_loading_response_error() {
        let status = EmbeddingStatus::Error {
            message: "Model not found".to_string(),
        };
        let result = embedding_loading_response(&status);
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();

        assert_eq!(json["status"], "error");
        assert_eq!(json["error"], "Model not found");
    }

    #[test]
    fn test_embedding_loading_response_ready() {
        let status = EmbeddingStatus::Ready;
        let result = embedding_loading_response(&status);
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();

        assert_eq!(json["status"], "ready");
    }
}
