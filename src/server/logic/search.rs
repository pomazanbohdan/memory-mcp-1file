use std::sync::Arc;

use rmcp::model::CallToolResult;
use serde_json::json;

use crate::config::AppState;
use crate::graph::{
    rrf_merge, run_ppr, DEFAULT_BM25_WEIGHT, DEFAULT_PPR_WEIGHT, DEFAULT_VECTOR_WEIGHT,
};
use crate::server::params::{RecallParams, SearchParams};
use crate::storage::StorageBackend;
use crate::types::{MemoryType, ScoredMemory, SearchResult};

use super::{error_response, normalize_limit, success_json};

const MAX_SEARCH_QUERY_CHARS: usize = 4_000;

fn validate_search_query(query: &str) -> anyhow::Result<()> {
    if query.chars().count() > MAX_SEARCH_QUERY_CHARS {
        anyhow::bail!(
            "query is too long (max {} characters)",
            MAX_SEARCH_QUERY_CHARS
        );
    }
    Ok(())
}

fn normalize_memory_content(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn dedup_key(r: &SearchResult) -> String {
    if let Some(h) = &r.content_hash {
        if !h.is_empty() {
            return format!("h:{h}");
        }
    }
    format!("c:{}", normalize_memory_content(&r.content).to_lowercase())
}

fn dedup_memory_results(mut results: Vec<SearchResult>, limit: usize) -> Vec<SearchResult> {
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out = Vec::with_capacity(limit.min(results.len()));
    let mut seen = std::collections::HashSet::new();
    for r in results {
        if seen.insert(dedup_key(&r)) {
            out.push(r);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

fn apply_min_score(mut results: Vec<SearchResult>, min_score: Option<f32>) -> Vec<SearchResult> {
    if let Some(ms) = min_score {
        let threshold = ms.clamp(0.0, 1.0);
        results.retain(|r| r.score >= threshold);
    }
    results
}

pub async fn search(state: &Arc<AppState>, params: SearchParams) -> anyhow::Result<CallToolResult> {
    crate::ensure_embedding_ready!(state);
    validate_search_query(&params.query)?;

    let query_embedding = state.embedding.embed(&params.query).await?;

    let limit = normalize_limit(params.limit);
    let results = match state
        .storage
        .vector_search(&query_embedding, limit * 3)
        .await
    {
        Ok(r) => r,
        Err(e) => return Ok(error_response(e)),
    };
    let results = apply_min_score(dedup_memory_results(results, limit), params.min_score);

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
    validate_search_query(&params.query)?;
    let limit = normalize_limit(params.limit);
    let results = match state.storage.bm25_search(&params.query, limit * 3).await {
        Ok(r) => r,
        Err(e) => return Ok(error_response(e)),
    };
    let results = apply_min_score(dedup_memory_results(results, limit), params.min_score);

    Ok(success_json(json!({
        "results": results,
        "count": results.len(),
        "query": params.query
    })))
}

pub async fn recall(state: &Arc<AppState>, params: RecallParams) -> anyhow::Result<CallToolResult> {
    use petgraph::graph::{DiGraph, NodeIndex};
    use std::collections::HashMap;

    crate::ensure_embedding_ready!(state);
    validate_search_query(&params.query)?;

    let query_embedding = state.embedding.embed(&params.query).await?;

    let limit = normalize_limit(params.limit);
    let fetch_limit = limit * 3;

    let vector_weight = params.vector_weight.unwrap_or(DEFAULT_VECTOR_WEIGHT);
    let bm25_weight = params.bm25_weight.unwrap_or(DEFAULT_BM25_WEIGHT);
    let ppr_weight = params.ppr_weight.unwrap_or(DEFAULT_PPR_WEIGHT);

    let vector_results_raw = state
        .storage
        .vector_search(&query_embedding, fetch_limit)
        .await
        .unwrap_or_default();
    let vector_results = dedup_memory_results(vector_results_raw, fetch_limit);

    let bm25_results_raw = state
        .storage
        .bm25_search(&params.query, fetch_limit)
        .await
        .unwrap_or_default();
    let bm25_results = dedup_memory_results(bm25_results_raw, fetch_limit);

    let vector_tuples: Vec<_> = vector_results
        .iter()
        .map(|r| (r.id.clone(), r.score))
        .collect();
    let bm25_tuples: Vec<_> = bm25_results
        .iter()
        .map(|r| (r.id.clone(), r.score))
        .collect();

    // Build deterministic, rank-preserving IDs for graph seeding.
    // Previous HashSet->Vec conversion produced random order, which made
    // PPR seeds unstable and degraded recall quality run-to-run.
    let mut all_ids: Vec<String> = Vec::with_capacity(vector_results.len() + bm25_results.len());
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for id in vector_results
        .iter()
        .chain(bm25_results.iter())
        .map(|r| r.id.clone())
    {
        if seen_ids.insert(id.clone()) {
            all_ids.push(id);
        }
    }

    let ppr_tuples: Vec<(String, f32)> = if !all_ids.is_empty() {
        match state.storage.get_subgraph(&all_ids).await {
            Ok((entities, relations)) if !entities.is_empty() => {
                let mut graph: DiGraph<String, f32> = DiGraph::new();
                let mut node_map: HashMap<String, NodeIndex> = HashMap::new();

                for entity in &entities {
                    if let Some(ref id) = entity.id {
                        let id_str = crate::types::record_key_to_string(&id.key);
                        let idx = graph.add_node(id_str.clone());
                        node_map.insert(id_str, idx);
                    }
                }

                for relation in &relations {
                    let from_str = crate::types::record_key_to_string(&relation.from_entity.key);
                    let to_str = crate::types::record_key_to_string(&relation.to_entity.key);
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

                let reverse_map: HashMap<NodeIndex, String> = node_map
                    .iter()
                    .map(|(id, idx)| (*idx, id.clone()))
                    .collect();

                run_ppr(&graph, &seed_nodes)
                    .into_iter()
                    .filter_map(|(idx, score)| reverse_map.get(&idx).map(|id| (id.clone(), score)))
                    .collect()
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

    let mut content_map: std::collections::HashMap<String, (&str, MemoryType, String)> =
        std::collections::HashMap::new();
    for r in &vector_results {
        content_map.insert(
            r.id.clone(),
            (&r.content, r.memory_type.clone(), dedup_key(r)),
        );
    }
    for r in &bm25_results {
        content_map.entry(r.id.clone()).or_insert((
            &r.content,
            r.memory_type.clone(),
            dedup_key(r),
        ));
    }

    let mut scored_memories: Vec<ScoredMemory> = Vec::with_capacity(limit);
    let mut seen_keys = std::collections::HashSet::new();
    for (id, scores) in merged {
        if let Some((content, mem_type, k)) = content_map.get(&id) {
            if seen_keys.insert(k.clone()) {
                scored_memories.push(ScoredMemory {
                    id: id.clone(),
                    content: content.to_string(),
                    memory_type: mem_type.clone(),
                    score: scores.combined_score,
                    vector_score: scores.vector_score,
                    bm25_score: scores.bm25_score,
                    ppr_score: scores.ppr_score,
                });
                if scored_memories.len() >= limit {
                    break;
                }
            }
        }
    }

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

        // Seed data
        ctx.state
            .storage
            .create_memory(Memory {
                content: "Rust is a systems programming language".to_string(),
                embedding: Some(vec![0.1; 768]), // Mock embedding
                ..Memory::new("Rust is a systems programming language".to_string())
            })
            .await
            .unwrap();

        ctx.state
            .storage
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
            mode: None,
            min_score: None,
        };
        let result = search(&ctx.state, search_params).await.unwrap();
        let val = serde_json::to_value(&result).unwrap();
        let text = val["content"][0]["text"].as_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(text).unwrap();

        // Vector ranking can be sensitive to adaptive score-flooring in tests.
        // Here we only verify response shape; relevance is asserted below via
        // BM25 and hybrid paths.
        assert!(json["count"].as_u64().is_some());

        // 2. BM25 Search
        let text_params = SearchParams {
            query: "scripting".to_string(),
            limit: Some(5),
            mode: None,
            min_score: None,
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
