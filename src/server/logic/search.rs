use std::sync::Arc;

use rmcp::model::CallToolResult;
use serde_json::json;

use crate::config::AppState;
use crate::graph::{
    apply_hub_dampening, personalized_page_rank, rrf_merge, DEFAULT_BM25_WEIGHT,
    DEFAULT_PPR_WEIGHT, DEFAULT_VECTOR_WEIGHT, PPR_DAMPING, PPR_MAX_ITER, PPR_TOLERANCE,
};
use crate::server::params::{RecallParams, SearchParams};
use crate::storage::StorageBackend;
use crate::types::{MemoryType, ScoredMemory};

use super::{error_response, normalize_limit, success_json};

pub async fn search(state: &Arc<AppState>, params: SearchParams) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);
    crate::ensure_embedding_ready!(state);

    let storage = state.storage().unwrap();
    let query_embedding = state.embedding.embed(&params.query).await?;

    let limit = normalize_limit(params.limit);
    let results = match storage.vector_search(&query_embedding, limit).await {
        Ok(r) => r,
        Err(e) => return Ok(error_response(e)),
    };

    Ok(success_json(json!({
        "results": results,
        "count": results.len(),
        "query": params.query
    })))
}

pub async fn search_text(
    state: &Arc<AppState>,
    params: SearchParams,
) -> anyhow::Result<CallToolResult> {
    crate::ensure_storage_ready!(state);

    let storage = state.storage().unwrap();
    let limit = normalize_limit(params.limit);
    let results = match storage.bm25_search(&params.query, limit).await {
        Ok(r) => r,
        Err(e) => return Ok(error_response(e)),
    };

    Ok(success_json(json!({
        "results": results,
        "count": results.len(),
        "query": params.query
    })))
}

pub async fn recall(state: &Arc<AppState>, params: RecallParams) -> anyhow::Result<CallToolResult> {
    use petgraph::graph::{DiGraph, NodeIndex};
    use std::collections::HashMap;

    crate::ensure_storage_ready!(state);
    crate::ensure_embedding_ready!(state);

    let storage = state.storage().unwrap();
    let query_embedding = state.embedding.embed(&params.query).await?;

    let limit = normalize_limit(params.limit);
    let fetch_limit = limit * 3;

    let vector_weight = params.vector_weight.unwrap_or(DEFAULT_VECTOR_WEIGHT);
    let bm25_weight = params.bm25_weight.unwrap_or(DEFAULT_BM25_WEIGHT);
    let ppr_weight = params.ppr_weight.unwrap_or(DEFAULT_PPR_WEIGHT);

    let vector_results = storage
        .vector_search(&query_embedding, fetch_limit)
        .await
        .unwrap_or_default();

    let bm25_results = storage
        .bm25_search(&params.query, fetch_limit)
        .await
        .unwrap_or_default();

    let vector_tuples: Vec<_> = vector_results
        .iter()
        .map(|r| (r.id.clone(), r.score))
        .collect();
    let bm25_tuples: Vec<_> = bm25_results
        .iter()
        .map(|r| (r.id.clone(), r.score))
        .collect();

    let all_ids: Vec<String> = vector_results
        .iter()
        .chain(bm25_results.iter())
        .map(|r| r.id.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let ppr_tuples: Vec<(String, f32)> = if !all_ids.is_empty() {
        match storage.get_subgraph(&all_ids).await {
            Ok((entities, relations)) if !entities.is_empty() => {
                let mut graph: DiGraph<String, f32> = DiGraph::new();
                let mut node_map: HashMap<String, NodeIndex> = HashMap::new();

                for entity in &entities {
                    if let Some(ref id) = entity.id {
                        let id_str = id.id.to_string();
                        let idx = graph.add_node(id_str.clone());
                        node_map.insert(id_str, idx);
                    }
                }

                for relation in &relations {
                    let from_str = relation.from_entity.id.to_string();
                    let to_str = relation.to_entity.id.to_string();
                    if let (Some(&from_idx), Some(&to_idx)) =
                        (node_map.get(&from_str), node_map.get(&to_str))
                    {
                        graph.add_edge(from_idx, to_idx, relation.weight);
                    }
                }

                let seed_nodes: Vec<NodeIndex> = all_ids
                    .iter()
                    .take(20)
                    .filter_map(|id| node_map.get(id).copied())
                    .collect();

                let mut ppr_scores = personalized_page_rank(
                    &graph,
                    &seed_nodes,
                    PPR_DAMPING,
                    PPR_TOLERANCE,
                    PPR_MAX_ITER,
                );

                let degrees: HashMap<NodeIndex, usize> = graph
                    .node_indices()
                    .map(|idx| (idx, graph.edges(idx).count()))
                    .collect();
                apply_hub_dampening(&mut ppr_scores, &degrees);

                let reverse_map: HashMap<NodeIndex, String> = node_map
                    .iter()
                    .map(|(id, idx)| (*idx, id.clone()))
                    .collect();

                let mut tuples: Vec<_> = ppr_scores
                    .into_iter()
                    .filter_map(|(idx, score)| reverse_map.get(&idx).map(|id| (id.clone(), score)))
                    .collect();
                tuples.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                tuples
            }
            _ => vec![],
        }
    } else {
        vec![]
    };

    let merged = rrf_merge(
        &vector_tuples,
        &bm25_tuples,
        &ppr_tuples,
        vector_weight,
        bm25_weight,
        ppr_weight,
        limit,
    );

    let mut content_map: std::collections::HashMap<String, (&str, MemoryType)> =
        std::collections::HashMap::new();
    for r in &vector_results {
        content_map.insert(r.id.clone(), (&r.content, r.memory_type.clone()));
    }
    for r in &bm25_results {
        content_map
            .entry(r.id.clone())
            .or_insert((&r.content, r.memory_type.clone()));
    }

    let scored_memories: Vec<ScoredMemory> = merged
        .into_iter()
        .filter_map(|(id, scores)| {
            content_map
                .get(&id)
                .map(|(content, mem_type)| ScoredMemory {
                    id: id.clone(),
                    content: content.to_string(),
                    memory_type: mem_type.clone(),
                    score: scores.combined_score,
                    vector_score: scores.vector_score,
                    bm25_score: scores.bm25_score,
                    ppr_score: scores.ppr_score,
                })
        })
        .collect();

    Ok(success_json(json!({
        "memories": scored_memories,
        "count": scored_memories.len(),
        "query": params.query,
        "weights": {
            "vector": vector_weight,
            "bm25": bm25_weight,
            "ppr": ppr_weight
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TestContext;
    use crate::types::Memory;

    #[tokio::test]
    async fn test_search_logic() {
        let ctx = TestContext::new().await;
        let storage = ctx
            .state
            .storage()
            .expect("Storage should be ready in tests");

        storage
            .create_memory(Memory {
                content: "Rust is a systems programming language".to_string(),
                embedding: Some(vec![0.1; 768]),
                ..Memory::new("Rust is a systems programming language".to_string())
            })
            .await
            .unwrap();

        storage
            .create_memory(Memory {
                content: "Python is great for scripting".to_string(),
                embedding: Some(vec![0.9; 768]),
                ..Memory::new("Python is great for scripting".to_string())
            })
            .await
            .unwrap();

        // 1. Vector Search
        let search_params = SearchParams {
            query: "Rust".to_string(),
            limit: Some(5),
        };
        let result = search(&ctx.state, search_params).await.unwrap();
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();

        // Mock embedding for "Rust" will match vec![0.1; 768] closer than vec![0.9]
        // Note: Mock embedding is deterministic based on hash.
        // We just check if we got results.
        assert!(json["count"].as_u64().unwrap() > 0);

        // 2. BM25 Search
        let text_params = SearchParams {
            query: "scripting".to_string(),
            limit: Some(5),
        };
        let result = search_text(&ctx.state, text_params).await.unwrap();
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();
        let content = json["results"][0]["content"].as_str().unwrap();
        assert!(content.contains("Python"));

        // 3. Recall (Hybrid)
        let recall_params = RecallParams {
            query: "systems".to_string(),
            limit: Some(5),
            vector_weight: None,
            bm25_weight: None,
            ppr_weight: None,
        };
        let result = recall(&ctx.state, recall_params).await.unwrap();
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(json["count"].as_u64().unwrap() > 0);
    }
}
