use std::collections::HashMap;
use std::path::Path;

use crate::types::Datetime;
use surrealdb::engine::local::{Db, SurrealKv};
use surrealdb::Surreal;

use super::StorageBackend;
use crate::graph::{GraphTraversalStorage, SymbolGraphTraversalStorage};
use crate::types::{
    CodeChunk, CodeSymbol, Direction, Entity, IndexStatus, ManifestEntry, Memory, MemoryUpdate,
    Relation, ScoredCodeChunk, SearchResult, SymbolRelation,
};
use crate::Result;

mod code_ops;
mod graph_ops;
mod helpers;
mod manifest_ops;
mod memory_ops;
mod symbol_ops;

pub struct SurrealStorage {
    pub(super) db: Surreal<Db>,
}

impl SurrealStorage {
    pub async fn new(data_dir: &Path, model_dim: usize) -> Result<Self> {
        let db_path = data_dir.join("db");
        std::fs::create_dir_all(&db_path)?;

        let db: Surreal<Db> = Surreal::new::<SurrealKv>(db_path).await?;
        db.use_ns("memory").use_db("main").await?;

        // Drop old fulltext index on code_chunks that caused startup errors on existing databases
        db.query("REMOVE INDEX IF EXISTS idx_chunks_fts ON code_chunks;")
            .await?;

        let schema = include_str!("../schema.surql").replace("{dim}", &model_dim.to_string());
        db.query(&schema).await?;

        Ok(Self { db })
    }

    pub async fn check_dimension(&self, expected: usize) -> Result<()> {
        let mut response = self.db.query("INFO FOR TABLE memories").await?;
        let result: Option<serde_json::Value> = response.take(0)?;

        if let Some(info) = result {
            if let Some(indexes) = info.get("indexes").and_then(|i| i.as_object()) {
                if let Some(idx_def) = indexes.get("idx_memories_vec").and_then(|v| v.as_str()) {
                    if let Some(dim) = self.extract_dimension(idx_def) {
                        if dim != expected {
                            tracing::warn!(
                                old = dim,
                                new = expected,
                                "Dimension mismatch detected, rebuilding vector indices"
                            );
                            self.rebuild_vector_indices(expected).await?;
                            self.db
                                .query(
                                    "UPDATE memories SET embedding_state = 'stale', embedding = NONE;
                                     UPDATE entities SET embedding = NONE;
                                     UPDATE code_chunks SET embedding = NONE;
                                     UPDATE code_symbols SET embedding = NONE;",
                                )
                                .await?;
                            tracing::info!("Indices rebuilt, old embeddings marked stale");
                            return Ok(());
                        }
                        tracing::info!(model = expected, db = dim, "Dimension check passed");
                        return Ok(());
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn rebuild_vector_indices(&self, dim: usize) -> Result<()> {
        let queries = format!(
            "REMOVE INDEX IF EXISTS idx_memories_vec ON memories;
             REMOVE INDEX IF EXISTS idx_entities_vec ON entities;
             REMOVE INDEX IF EXISTS idx_chunks_vec ON code_chunks;
             REMOVE INDEX IF EXISTS idx_symbols_vec ON code_symbols;
             DEFINE INDEX idx_memories_vec ON memories FIELDS embedding HNSW DIMENSION {d} DIST COSINE;
             DEFINE INDEX idx_entities_vec ON entities FIELDS embedding HNSW DIMENSION {d} DIST COSINE;
             DEFINE INDEX idx_chunks_vec ON code_chunks FIELDS embedding HNSW DIMENSION {d} DIST COSINE;
             DEFINE INDEX idx_symbols_vec ON code_symbols FIELDS embedding HNSW DIMENSION {d} DIST COSINE;",
            d = dim
        );
        self.db.query(&queries).await?;
        Ok(())
    }

    fn extract_dimension(&self, def: &str) -> Option<usize> {
        def.split("DIMENSION ")
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }

    /// Directly update embedding fields for a memory (stale re-embed).
    pub async fn raw_update_embedding(
        &self,
        id: &str,
        embedding: Vec<f32>,
        content_hash: String,
        embedding_state: &str,
    ) -> Result<()> {
        memory_ops::raw_update_embedding(&self.db, id, embedding, content_hash, embedding_state)
            .await
    }

    /// Get all memories with stale or missing embeddings.
    pub async fn get_stale_memories(&self) -> Result<Vec<Memory>> {
        let mut response = self
            .db
            .query("SELECT * FROM memories WHERE embedding_state = 'stale' OR embedding IS NONE")
            .await?;
        let memories: Vec<Memory> = response.take(0).unwrap_or_default();
        Ok(memories)
    }
}

// ---------------------------------------------------------------------------
// GraphTraversalStorage — needed by the graph traversal engine
// ---------------------------------------------------------------------------

impl GraphTraversalStorage for SurrealStorage {
    async fn get_direct_relations(
        &self,
        entity_id: &str,
        direction: Direction,
    ) -> Result<(Vec<Entity>, Vec<Relation>)> {
        use crate::types::ThingId;

        let entity_thing = ThingId::new("entities", entity_id)?.to_string();

        let sql = match direction {
            Direction::Outgoing => "SELECT * FROM relations WHERE `in` = (type::record($entity_id))",
            Direction::Incoming => "SELECT * FROM relations WHERE `out` = (type::record($entity_id))",
            Direction::Both => {
                "SELECT * FROM relations WHERE `in` = (type::record($entity_id)) OR `out` = (type::record($entity_id))"
            }
        };

        let mut response = self
            .db
            .query(sql)
            .bind(("entity_id", entity_thing.clone()))
            .await?;

        // Use Value intermediary to bypass SurrealValue RecordId bug
        let raw: surrealdb_types::Value = response.take(0)?;
        let relations = helpers::value_to_relations(raw);

        let mut entity_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for rel in &relations {
            match direction {
                Direction::Outgoing => {
                    entity_ids.insert(crate::types::record_key_to_string(&rel.to_entity.key));
                }
                Direction::Incoming => {
                    entity_ids.insert(crate::types::record_key_to_string(&rel.from_entity.key));
                }
                Direction::Both => {
                    let from_id = crate::types::record_key_to_string(&rel.from_entity.key);
                    let to_id = crate::types::record_key_to_string(&rel.to_entity.key);
                    if from_id != entity_id {
                        entity_ids.insert(from_id);
                    }
                    if to_id != entity_id {
                        entity_ids.insert(to_id);
                    }
                }
            }
        }

        let entity_ids_vec: Vec<String> = entity_ids.into_iter().collect();
        let entity_sql = "SELECT * FROM entities WHERE meta::id(id) IN $ids";
        let mut entity_response = self
            .db
            .query(entity_sql)
            .bind(("ids", entity_ids_vec))
            .await?;
        let entities: Vec<Entity> = entity_response.take(0)?;

        Ok((entities, relations))
    }

    async fn get_direct_relations_batch(
        &self,
        entity_ids: &[String],
        direction: Direction,
    ) -> Result<(Vec<Entity>, Vec<Relation>)> {
        if entity_ids.is_empty() {
            return Ok((vec![], vec![]));
        }

        let things: Vec<crate::types::Thing> = entity_ids
            .iter()
            .map(|id| {
                use crate::types::ThingId;
                ThingId::new("entities", id).map(|t| t.to_thing())
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let sql = match direction {
            Direction::Outgoing => "SELECT * FROM relations WHERE `in` IN $entity_ids",
            Direction::Incoming => "SELECT * FROM relations WHERE `out` IN $entity_ids",
            Direction::Both => {
                "SELECT * FROM relations WHERE `in` IN $entity_ids OR `out` IN $entity_ids"
            }
        };

        let mut response = self.db.query(sql).bind(("entity_ids", things)).await?;

        let raw: surrealdb_types::Value = response.take(0)?;
        let relations = helpers::value_to_relations(raw);

        let source_ids: std::collections::HashSet<&String> = entity_ids.iter().collect();
        let mut new_entity_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for rel in &relations {
            let from_id = crate::types::record_key_to_string(&rel.from_entity.key);
            let to_id = crate::types::record_key_to_string(&rel.to_entity.key);

            if !source_ids.contains(&from_id) {
                new_entity_ids.insert(from_id);
            }
            if !source_ids.contains(&to_id) {
                new_entity_ids.insert(to_id);
            }
        }

        let mut entities: Vec<Entity> = vec![];
        for eid in new_entity_ids {
            if let Some(entity) = self.get_entity(&eid).await? {
                entities.push(entity);
            }
        }

        Ok((entities, relations))
    }
}

// ---------------------------------------------------------------------------
// SymbolGraphTraversalStorage — needed by the symbol graph traversal engine
// ---------------------------------------------------------------------------

impl SymbolGraphTraversalStorage for SurrealStorage {
    async fn get_direct_symbol_relations(
        &self,
        symbol_id: &str,
        direction: Direction,
    ) -> Result<(Vec<CodeSymbol>, Vec<SymbolRelation>)> {
        // Delegate to the existing single-hop implementation
        symbol_ops::get_related_symbols(&self.db, symbol_id, 1, direction).await
    }

    async fn get_direct_symbol_relations_batch(
        &self,
        symbol_ids: &[String],
        direction: Direction,
    ) -> Result<(Vec<CodeSymbol>, Vec<SymbolRelation>)> {
        if symbol_ids.is_empty() {
            return Ok((vec![], vec![]));
        }

        // Build Thing IDs for the batch query
        let things: Vec<crate::types::Thing> = symbol_ids
            .iter()
            .filter_map(|id| {
                let (table, key) = if let Some(idx) = id.find(':') {
                    (&id[..idx], &id[idx + 1..])
                } else {
                    ("code_symbols", id.as_str())
                };
                use crate::types::ThingId;
                ThingId::new(table, key).ok().map(|t| t.to_thing())
            })
            .collect();

        if things.is_empty() {
            return Ok((vec![], vec![]));
        }

        let sql = match direction {
            Direction::Outgoing => "SELECT * FROM symbol_relation WHERE `in` IN $ids",
            Direction::Incoming => "SELECT * FROM symbol_relation WHERE `out` IN $ids",
            Direction::Both => "SELECT * FROM symbol_relation WHERE `in` IN $ids OR `out` IN $ids",
        };

        let mut response = self.db.query(sql).bind(("ids", things)).await?;

        let raw: surrealdb_types::Value = response.take(0)?;
        let relations = helpers::value_to_symbol_relations(raw);

        // Collect target symbol IDs from relations (excluding source IDs)
        let source_set: std::collections::HashSet<&String> = symbol_ids.iter().collect();
        let mut target_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for rel in &relations {
            let from_str = format!(
                "{}:{}",
                rel.from_symbol.table.as_str(),
                crate::types::record_key_to_string(&rel.from_symbol.key)
            );
            let to_str = format!(
                "{}:{}",
                rel.to_symbol.table.as_str(),
                crate::types::record_key_to_string(&rel.to_symbol.key)
            );

            match direction {
                Direction::Outgoing => {
                    if !source_set.contains(&to_str) {
                        target_ids.insert(to_str);
                    }
                }
                Direction::Incoming => {
                    if !source_set.contains(&from_str) {
                        target_ids.insert(from_str);
                    }
                }
                Direction::Both => {
                    if !source_set.contains(&from_str) {
                        target_ids.insert(from_str);
                    }
                    if !source_set.contains(&to_str) {
                        target_ids.insert(to_str);
                    }
                }
            }
        }

        // Batch-fetch symbols
        let symbols: Vec<CodeSymbol> = if target_ids.is_empty() {
            vec![]
        } else {
            let record_list: Vec<String> = target_ids
                .into_iter()
                .map(|sid| {
                    if sid.contains(':') {
                        sid
                    } else {
                        format!("code_symbols:{}", sid)
                    }
                })
                .collect();
            let sql = format!("SELECT * FROM {}", record_list.join(", "));
            let mut response = self.db.query(sql).await?;
            response.take(0).unwrap_or_default()
        };

        Ok((symbols, relations))
    }
}

// ---------------------------------------------------------------------------
// StorageBackend — single impl block, delegates to sub-module free functions
// ---------------------------------------------------------------------------

#[allow(async_fn_in_trait)]
impl StorageBackend for SurrealStorage {
    async fn create_memory(&self, memory: Memory) -> Result<String> {
        memory_ops::create_memory(&self.db, memory).await
    }

    async fn get_memory(&self, id: &str) -> Result<Option<Memory>> {
        memory_ops::get_memory(&self.db, id).await
    }

    async fn update_memory(&self, id: &str, update: MemoryUpdate) -> Result<Memory> {
        memory_ops::update_memory(&self.db, id, update).await
    }

    async fn delete_memory(&self, id: &str) -> Result<bool> {
        memory_ops::delete_memory(&self.db, id).await
    }

    async fn list_memories(&self, limit: usize, offset: usize) -> Result<Vec<Memory>> {
        memory_ops::list_memories(&self.db, limit, offset).await
    }

    async fn count_memories(&self) -> Result<usize> {
        memory_ops::count_memories(&self.db).await
    }

    async fn vector_search(&self, embedding: &[f32], limit: usize) -> Result<Vec<SearchResult>> {
        memory_ops::vector_search(&self.db, embedding, limit).await
    }

    async fn vector_search_code(
        &self,
        embedding: &[f32],
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ScoredCodeChunk>> {
        symbol_ops::vector_search_code(&self.db, embedding, project_id, limit).await
    }

    async fn vector_search_symbols(
        &self,
        embedding: &[f32],
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodeSymbol>> {
        symbol_ops::vector_search_symbols(&self.db, embedding, project_id, limit).await
    }

    async fn bm25_search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        memory_ops::bm25_search(&self.db, query, limit).await
    }

    async fn bm25_search_code(
        &self,
        query: &str,
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ScoredCodeChunk>> {
        code_ops::bm25_search_code(&self.db, query, project_id, limit).await
    }

    async fn create_entity(&self, entity: Entity) -> Result<String> {
        graph_ops::create_entity(&self.db, entity).await
    }

    async fn get_entity(&self, id: &str) -> Result<Option<Entity>> {
        graph_ops::get_entity(&self.db, id).await
    }

    async fn search_entities(&self, query: &str, limit: usize) -> Result<Vec<Entity>> {
        graph_ops::search_entities(&self.db, query, limit).await
    }

    async fn create_relation(&self, relation: Relation) -> Result<String> {
        graph_ops::create_relation(&self.db, relation).await
    }

    async fn get_related(
        &self,
        entity_id: &str,
        depth: usize,
        direction: Direction,
    ) -> Result<(Vec<Entity>, Vec<Relation>)> {
        graph_ops::get_related(&self.db, entity_id, depth, direction).await
    }

    async fn get_subgraph(&self, entity_ids: &[String]) -> Result<(Vec<Entity>, Vec<Relation>)> {
        graph_ops::get_subgraph(&self.db, entity_ids).await
    }

    async fn get_node_degrees(&self, entity_ids: &[String]) -> Result<HashMap<String, usize>> {
        graph_ops::get_node_degrees(&self.db, entity_ids).await
    }

    async fn get_all_entities(&self) -> Result<Vec<Entity>> {
        graph_ops::get_all_entities(&self.db).await
    }

    async fn get_all_relations(&self) -> Result<Vec<Relation>> {
        graph_ops::get_all_relations(&self.db).await
    }

    async fn get_valid(&self, user_id: Option<&str>, limit: usize) -> Result<Vec<Memory>> {
        memory_ops::get_valid(&self.db, user_id, limit).await
    }

    async fn get_valid_at(
        &self,
        timestamp: Datetime,
        user_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        memory_ops::get_valid_at(&self.db, timestamp, user_id, limit).await
    }

    async fn invalidate(
        &self,
        id: &str,
        reason: Option<&str>,
        superseded_by: Option<&str>,
    ) -> Result<bool> {
        memory_ops::invalidate(&self.db, id, reason, superseded_by).await
    }

    async fn create_code_chunk(&self, chunk: CodeChunk) -> Result<String> {
        code_ops::create_code_chunk(&self.db, chunk).await
    }

    async fn create_code_chunks_batch(
        &self,
        chunks: Vec<CodeChunk>,
    ) -> Result<Vec<(String, CodeChunk)>> {
        code_ops::create_code_chunks_batch(&self.db, chunks).await
    }

    async fn delete_project_chunks(&self, project_id: &str) -> Result<usize> {
        code_ops::delete_project_chunks(&self.db, project_id).await
    }

    async fn delete_chunks_by_path(&self, project_id: &str, file_path: &str) -> Result<usize> {
        code_ops::delete_chunks_by_path(&self.db, project_id, file_path).await
    }

    async fn get_chunks_by_path(
        &self,
        project_id: &str,
        file_path: &str,
    ) -> Result<Vec<CodeChunk>> {
        code_ops::get_chunks_by_path(&self.db, project_id, file_path).await
    }

    async fn get_all_chunks_for_project(&self, project_id: &str) -> Result<Vec<CodeChunk>> {
        code_ops::get_all_chunks_for_project(&self.db, project_id).await
    }

    async fn get_chunks_paginated(
        &self,
        project_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<CodeChunk>> {
        code_ops::get_chunks_paginated(&self.db, project_id, limit, offset).await
    }

    async fn get_chunks_by_ids(&self, ids: &[String]) -> Result<Vec<CodeChunk>> {
        code_ops::get_chunks_by_ids(&self.db, ids).await
    }

    async fn clear_project_embeddings(&self, project_id: &str) -> Result<u64> {
        code_ops::clear_project_embeddings(&self.db, project_id).await
    }

    async fn get_index_status(&self, project_id: &str) -> Result<Option<IndexStatus>> {
        code_ops::get_index_status(&self.db, project_id).await
    }

    async fn update_index_status(&self, status: IndexStatus) -> Result<()> {
        code_ops::update_index_status(&self.db, status).await
    }

    async fn delete_index_status(&self, project_id: &str) -> Result<()> {
        code_ops::delete_index_status(&self.db, project_id).await
    }

    async fn list_projects(&self) -> Result<Vec<String>> {
        code_ops::list_projects(&self.db).await
    }

    async fn get_file_hash(&self, project_id: &str, file_path: &str) -> Result<Option<String>> {
        code_ops::get_file_hash(&self.db, project_id, file_path).await
    }

    async fn set_file_hash(&self, project_id: &str, file_path: &str, hash: &str) -> Result<()> {
        code_ops::set_file_hash(&self.db, project_id, file_path, hash).await
    }

    async fn set_file_hashes_batch(
        &self,
        project_id: &str,
        hashes: &[(String, String)],
    ) -> Result<()> {
        code_ops::set_file_hashes_batch(&self.db, project_id, hashes).await
    }

    async fn delete_file_hashes(&self, project_id: &str) -> Result<()> {
        code_ops::delete_file_hashes(&self.db, project_id).await
    }

    async fn delete_file_hash(&self, project_id: &str, file_path: &str) -> Result<()> {
        code_ops::delete_file_hash(&self.db, project_id, file_path).await
    }

    async fn create_code_symbol(&self, symbol: CodeSymbol) -> Result<String> {
        symbol_ops::create_code_symbol(&self.db, symbol).await
    }

    async fn create_code_symbols_batch(&self, symbols: Vec<CodeSymbol>) -> Result<Vec<String>> {
        symbol_ops::create_code_symbols_batch(&self.db, symbols).await
    }

    async fn update_symbol_embedding(&self, id: &str, embedding: Vec<f32>) -> Result<()> {
        symbol_ops::update_symbol_embedding(&self.db, id, embedding).await
    }

    async fn update_chunk_embedding(&self, id: &str, embedding: Vec<f32>) -> Result<()> {
        code_ops::update_chunk_embedding(&self.db, id, embedding).await
    }

    async fn batch_update_symbol_embeddings(&self, updates: &[(String, Vec<f32>)]) -> Result<()> {
        symbol_ops::batch_update_symbol_embeddings(&self.db, updates).await
    }

    async fn batch_update_chunk_embeddings(&self, updates: &[(String, Vec<f32>)]) -> Result<()> {
        code_ops::batch_update_chunk_embeddings(&self.db, updates).await
    }

    async fn create_symbol_relation(&self, relation: SymbolRelation) -> Result<String> {
        symbol_ops::create_symbol_relation(&self.db, relation).await
    }

    async fn create_symbol_relations_batch(&self, relations: Vec<SymbolRelation>) -> Result<u32> {
        symbol_ops::create_symbol_relations_batch(&self.db, relations).await
    }

    async fn delete_project_symbols(&self, project_id: &str) -> Result<usize> {
        symbol_ops::delete_project_symbols(&self.db, project_id).await
    }

    async fn delete_symbols_by_path(&self, project_id: &str, file_path: &str) -> Result<usize> {
        symbol_ops::delete_symbols_by_path(&self.db, project_id, file_path).await
    }

    async fn get_project_symbols(&self, project_id: &str) -> Result<Vec<CodeSymbol>> {
        symbol_ops::get_project_symbols(&self.db, project_id).await
    }

    async fn get_symbol_callers(&self, symbol_id: &str) -> Result<Vec<CodeSymbol>> {
        symbol_ops::get_symbol_callers(&self.db, symbol_id).await
    }

    async fn get_symbol_callees(&self, symbol_id: &str) -> Result<Vec<CodeSymbol>> {
        symbol_ops::get_symbol_callees(&self.db, symbol_id).await
    }

    async fn get_related_symbols(
        &self,
        symbol_id: &str,
        depth: usize,
        direction: Direction,
    ) -> Result<(Vec<CodeSymbol>, Vec<SymbolRelation>)> {
        symbol_ops::get_related_symbols(&self.db, symbol_id, depth, direction).await
    }

    async fn get_code_subgraph(
        &self,
        symbol_ids: &[String],
    ) -> Result<(Vec<CodeSymbol>, Vec<SymbolRelation>)> {
        symbol_ops::get_code_subgraph(&self.db, symbol_ids).await
    }

    async fn search_symbols(
        &self,
        query: &str,
        project_id: Option<&str>,
        limit: usize,
        offset: usize,
        symbol_type: Option<&str>,
        path_prefix: Option<&str>,
    ) -> Result<(Vec<CodeSymbol>, u32)> {
        symbol_ops::search_symbols(
            &self.db,
            query,
            project_id,
            limit,
            offset,
            symbol_type,
            path_prefix,
        )
        .await
    }

    async fn replace_symbol_chunk_map(
        &self,
        project_id: &str,
        rows: &[(String, String, f32)],
    ) -> Result<u32> {
        symbol_ops::replace_symbol_chunk_map(&self.db, project_id, rows).await
    }

    async fn get_mapped_chunks_for_symbols(
        &self,
        project_id: &str,
        symbol_ids: &[String],
        limit: usize,
    ) -> Result<Vec<(String, f32)>> {
        symbol_ops::get_mapped_chunks_for_symbols(&self.db, project_id, symbol_ids, limit).await
    }

    async fn count_symbols(&self, project_id: &str) -> Result<u32> {
        symbol_ops::count_symbols(&self.db, project_id).await
    }

    async fn count_chunks(&self, project_id: &str) -> Result<u32> {
        code_ops::count_chunks(&self.db, project_id).await
    }

    async fn count_embedded_symbols(&self, project_id: &str) -> Result<u32> {
        symbol_ops::count_embedded_symbols(&self.db, project_id).await
    }

    async fn count_embedded_chunks(&self, project_id: &str) -> Result<u32> {
        code_ops::count_embedded_chunks(&self.db, project_id).await
    }

    async fn get_unembedded_chunks(&self, project_id: &str) -> Result<Vec<(String, String)>> {
        code_ops::get_unembedded_chunks(&self.db, project_id).await
    }

    async fn get_unembedded_symbols(&self, project_id: &str) -> Result<Vec<(String, String)>> {
        symbol_ops::get_unembedded_symbols(&self.db, project_id).await
    }

    async fn count_symbol_relations(&self, project_id: &str) -> Result<u32> {
        symbol_ops::count_symbol_relations(&self.db, project_id).await
    }

    async fn find_symbol_by_name(
        &self,
        project_id: &str,
        name: &str,
    ) -> Result<Option<CodeSymbol>> {
        symbol_ops::find_symbol_by_name(&self.db, project_id, name).await
    }

    async fn find_symbols_by_names(
        &self,
        project_id: &str,
        names: &[String],
    ) -> Result<Vec<CodeSymbol>> {
        symbol_ops::find_symbols_by_names(&self.db, project_id, names).await
    }

    async fn find_symbol_by_name_with_context(
        &self,
        project_id: &str,
        name: &str,
        prefer_file: Option<&str>,
    ) -> Result<Option<CodeSymbol>> {
        symbol_ops::find_symbol_by_name_with_context(&self.db, project_id, name, prefer_file).await
    }

    async fn health_check(&self) -> Result<bool> {
        self.db.query("INFO FOR DB").await?;
        Ok(true)
    }

    async fn reset_db(&self) -> Result<()> {
        // Run each DELETE independently — some tables may not exist yet
        // (e.g. relation tables are created on first RELATE).
        // Using a transaction would cause one failure to cancel all DELETEEs.
        let tables = [
            "memories",
            "entities",
            "relations",
            "code_chunks",
            "code_symbols",
            "symbol_chunk_map",
            "symbol_relation",
            "index_status",
            "file_hashes",
        ];
        for table in &tables {
            let _ = self.db.query(format!("DELETE {}", table)).await;
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        // Force WAL flush: SELECT count() touches the storage engine,
        // ensuring pending writes from any table are committed to disk.
        self.db
            .query(
                "SELECT count() AS c FROM memories GROUP ALL;
                 SELECT count() AS c FROM entities GROUP ALL;
                 SELECT count() AS c FROM code_chunks GROUP ALL;",
            )
            .await?;
        tracing::info!("Storage flushed successfully");
        Ok(())
    }

    async fn upsert_manifest_entry(&self, project_id: &str, file_path: &str) -> Result<()> {
        manifest_ops::upsert_manifest_entry(&self.db, project_id, file_path).await
    }

    async fn upsert_manifest_entries(&self, project_id: &str, file_paths: &[String]) -> Result<()> {
        manifest_ops::upsert_manifest_entries(&self.db, project_id, file_paths).await
    }

    async fn get_manifest_entries(&self, project_id: &str) -> Result<Vec<ManifestEntry>> {
        manifest_ops::get_manifest_entries(&self.db, project_id).await
    }

    async fn delete_manifest_entries(&self, project_id: &str) -> Result<()> {
        manifest_ops::delete_manifest_entries(&self.db, project_id).await
    }

    async fn delete_manifest_entry(&self, project_id: &str, file_path: &str) -> Result<()> {
        manifest_ops::delete_manifest_entry(&self.db, project_id, file_path).await
    }

    async fn count_manifest_entries(&self, project_id: &str) -> Result<usize> {
        manifest_ops::count_manifest_entries(&self.db, project_id).await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        ChunkType, Datetime, Entity, Language, Memory, MemoryType, MemoryUpdate, RecordId, Relation,
    };
    use tempfile::tempdir;

    async fn setup_test_db() -> (SurrealStorage, tempfile::TempDir) {
        let tmp = tempdir().unwrap();
        let storage = SurrealStorage::new(tmp.path(), 768).await.unwrap();
        (storage, tmp)
    }

    #[tokio::test]
    async fn test_memory_crud() {
        let (storage, _tmp) = setup_test_db().await;

        let memory = Memory {
            id: None,
            content: "Test memory content".to_string(),
            embedding: Some(vec![0.1; 768]),
            memory_type: MemoryType::Semantic,
            user_id: Some("user1".to_string()),
            metadata: None,
            event_time: Datetime::default(),
            ingestion_time: Datetime::default(),
            valid_from: Datetime::default(),
            valid_until: None,
            importance_score: 1.0,
            invalidation_reason: None,
            content_hash: None,
            embedding_state: Default::default(),
        };

        let id = storage.create_memory(memory.clone()).await.unwrap();
        assert!(!id.is_empty());

        let retrieved = storage
            .get_memory(&id)
            .await
            .unwrap()
            .expect("Memory not found");
        assert_eq!(retrieved.content, memory.content);
        assert_eq!(retrieved.user_id, memory.user_id);

        let update = MemoryUpdate {
            content: Some("Updated content".to_string()),
            memory_type: None,
            metadata: None,
            embedding: None,
            content_hash: None,
            embedding_state: None,
        };
        let updated = storage.update_memory(&id, update).await.unwrap();
        assert_eq!(updated.content, "Updated content");

        let list = storage.list_memories(10, 0).await.unwrap();
        assert_eq!(list.len(), 1);

        let deleted = storage.delete_memory(&id).await.unwrap();
        assert!(deleted);
        assert!(storage.get_memory(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_bm25_search() {
        let (storage, _tmp) = setup_test_db().await;

        storage
            .create_memory(Memory {
                id: None,
                content: "Rust programming language".to_string(),
                embedding: Some(vec![0.0; 768]),
                memory_type: MemoryType::Semantic,
                user_id: None,
                metadata: None,
                event_time: Datetime::default(),
                ingestion_time: Datetime::default(),
                valid_from: Datetime::default(),
                valid_until: None,
                importance_score: 1.0,
                invalidation_reason: None,
                content_hash: None,
                embedding_state: Default::default(),
            })
            .await
            .unwrap();

        storage
            .create_memory(Memory {
                id: None,
                content: "Python scripting".to_string(),
                embedding: Some(vec![0.0; 768]),
                memory_type: MemoryType::Semantic,
                user_id: None,
                metadata: None,
                event_time: Datetime::default(),
                ingestion_time: Datetime::default(),
                valid_from: Datetime::default(),
                valid_until: None,
                importance_score: 1.0,
                invalidation_reason: None,
                content_hash: None,
                embedding_state: Default::default(),
            })
            .await
            .unwrap();

        let results = storage.bm25_search("Rust", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("Rust"));
        // Score must be > 0 (not all-zero flat scores from broken search::score())
        assert!(
            results[0].score > 0.0,
            "Score should be > 0, got {}",
            results[0].score
        );
    }

    #[tokio::test]
    async fn test_entity_and_relation() {
        let (storage, _tmp) = setup_test_db().await;

        let e1_id = storage
            .create_entity(Entity {
                id: None,
                name: "Entity 1".to_string(),
                entity_type: "person".to_string(),
                description: None,
                embedding: None,
                content_hash: None,
                user_id: None,
                created_at: Datetime::default(),
            })
            .await
            .unwrap();

        let e2_id = storage
            .create_entity(Entity {
                id: None,
                name: "Entity 2".to_string(),
                entity_type: "place".to_string(),
                description: None,
                embedding: None,
                content_hash: None,
                user_id: None,
                created_at: Datetime::default(),
            })
            .await
            .unwrap();

        let _rel_id = storage
            .create_relation(Relation {
                id: None,
                from_entity: RecordId::new("entities", e1_id.clone()),
                to_entity: RecordId::new("entities", e2_id.clone()),
                relation_type: "lives_in".to_string(),
                weight: 1.0,
                valid_from: Datetime::default(),
                valid_until: None,
            })
            .await
            .unwrap();

        let (related, _rels_out) = storage
            .get_related(&e1_id, 1, Direction::Outgoing)
            .await
            .unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].name, "Entity 2");
    }

    #[tokio::test]
    async fn test_symbol_call_hierarchy() {
        let (storage, _tmp) = setup_test_db().await;
        use crate::types::{CodeRelationType, CodeSymbol, SymbolRelation, SymbolType};

        // 1. Create Symbols: Caller -> Callee
        let caller = CodeSymbol::new(
            "main".to_string(),
            SymbolType::Function,
            "main.rs".to_string(),
            1,
            5,
            "test_project".to_string(),
        );
        let caller_id = storage.create_code_symbol(caller).await.unwrap();

        let callee = CodeSymbol::new(
            "helper".to_string(),
            SymbolType::Function,
            "helper.rs".to_string(),
            10,
            15,
            "test_project".to_string(),
        );
        let callee_id = storage.create_code_symbol(callee).await.unwrap();

        // 2. Create Relation: main calls helper
        let caller_key = caller_id
            .strip_prefix("code_symbols:")
            .unwrap_or(&caller_id);
        let callee_key = callee_id
            .strip_prefix("code_symbols:")
            .unwrap_or(&callee_id);

        let relation = SymbolRelation::new(
            crate::types::RecordId::new("code_symbols", caller_key.to_string()),
            crate::types::RecordId::new("code_symbols", callee_key.to_string()),
            CodeRelationType::Calls,
            "main.rs".to_string(),
            3,
            "test_project".to_string(),
        );
        storage.create_symbol_relation(relation).await.unwrap();

        // 3. Test get_symbol_callees (Outgoing)
        // main -> ? (should be helper)
        let callees = storage.get_symbol_callees(&caller_id).await.unwrap();
        assert_eq!(callees.len(), 1, "Should find 1 callee");
        assert_eq!(callees[0].name, "helper");

        // 4. Test get_symbol_callers (Incoming)
        // ? -> helper (should be main)
        let callers = storage.get_symbol_callers(&callee_id).await.unwrap();
        assert_eq!(callers.len(), 1, "Should find 1 caller");
        assert_eq!(callers[0].name, "main");
    }

    #[tokio::test]
    async fn test_temporal_validation() {
        let (storage, _tmp) = setup_test_db().await;

        let id = storage
            .create_memory(Memory {
                id: None,
                content: "Temporary memory".to_string(),
                embedding: Some(vec![0.0; 768]),
                memory_type: MemoryType::Semantic,
                user_id: None,
                metadata: None,
                event_time: Datetime::default(),
                ingestion_time: Datetime::default(),
                valid_from: Datetime::default(),
                valid_until: None,
                importance_score: 1.0,
                invalidation_reason: None,
                content_hash: None,
                embedding_state: Default::default(),
            })
            .await
            .unwrap();

        let valid = storage.get_valid(None, 10).await.unwrap();
        assert_eq!(valid.len(), 1);

        storage
            .invalidate(&id, Some("test reason"), None)
            .await
            .unwrap();

        let valid_after = storage.get_valid(None, 10).await.unwrap();
        assert_eq!(valid_after.len(), 0);
    }

    #[tokio::test]
    async fn test_reset_db() {
        let (storage, _tmp) = setup_test_db().await;

        storage
            .create_memory(Memory {
                id: None,
                content: "To be deleted".to_string(),
                embedding: None,
                memory_type: MemoryType::Semantic,
                user_id: None,
                metadata: None,
                event_time: Datetime::default(),
                ingestion_time: Datetime::default(),
                valid_from: Datetime::default(),
                valid_until: None,
                importance_score: 1.0,
                invalidation_reason: None,
                content_hash: None,
                embedding_state: Default::default(),
            })
            .await
            .unwrap();

        assert_eq!(storage.count_memories().await.unwrap(), 1);

        storage.reset_db().await.unwrap();

        assert_eq!(storage.count_memories().await.unwrap(), 0);

        // file_hashes metadata must also be cleared by global reset
        let hashes: Vec<surrealdb::sql::Value> = storage
            .db
            .query("SELECT * FROM file_hashes")
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert!(hashes.is_empty());
    }

    #[tokio::test]
    async fn test_bulk_insert_code_chunks() {
        let (storage, _tmp) = setup_test_db().await;
        use crate::types::{ChunkType, CodeChunk, Language};

        let chunks: Vec<CodeChunk> = (0..50)
            .map(|i| CodeChunk {
                id: None,
                file_path: format!("src/file_{}.rs", i),
                content: format!("fn test_{}() {{}}", i),
                language: Language::Rust,
                start_line: 1,
                end_line: 3,
                chunk_type: ChunkType::Function,
                name: Some(format!("test_{}", i)),
                context_path: None,
                embedding: Some(vec![0.1; 768]),
                content_hash: format!("hash_{}", i),
                project_id: Some("test_project".to_string()),
                indexed_at: Datetime::default(),
            })
            .collect();

        let results = storage.create_code_chunks_batch(chunks).await.unwrap();
        assert_eq!(results.len(), 50);

        let _status = storage.get_index_status("test_project").await.unwrap();
        // that's handled by the indexer. But we can verify chunks exist.

        let results = storage
            .bm25_search_code("test", Some("test_project"), 100)
            .await
            .unwrap();
        assert_eq!(results.len(), 50);
    }

    #[tokio::test]
    async fn test_batch_update_embeddings() {
        let (storage, _tmp) = setup_test_db().await;

        let chunks: Vec<CodeChunk> = (0..5)
            .map(|i| CodeChunk {
                id: None,
                file_path: format!("src/embed_{}.rs", i),
                content: format!("fn embed_{}() {{}}", i),
                language: Language::Rust,
                start_line: 1,
                end_line: 3,
                chunk_type: ChunkType::Function,
                name: Some(format!("embed_{}", i)),
                context_path: None,
                embedding: None,
                content_hash: format!("embed_hash_{}", i),
                project_id: Some("embed_project".to_string()),
                indexed_at: Datetime::default(),
            })
            .collect();

        let results = storage.create_code_chunks_batch(chunks).await.unwrap();
        assert_eq!(results.len(), 5);

        let chunk_ids: Vec<String> = results.iter().map(|(id, _)| id.clone()).collect();

        let updates: Vec<(String, Vec<f32>)> = chunk_ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), vec![i as f32 * 0.1; 768]))
            .collect();

        storage
            .batch_update_chunk_embeddings(&updates)
            .await
            .unwrap();

        let search_results = storage
            .bm25_search_code("embed", Some("embed_project"), 10)
            .await
            .unwrap();
        assert_eq!(search_results.len(), 5);
    }

    #[tokio::test]
    async fn test_search_symbols_with_type_filter() {
        let (storage, _tmp) = setup_test_db().await;
        use crate::types::{CodeSymbol, SymbolType};

        // Insert function symbols
        for i in 0..3 {
            let sym = CodeSymbol::new(
                format!("my_func_{}", i),
                SymbolType::Function,
                format!("src/file_{}.rs", i),
                1,
                10,
                "proj1".to_string(),
            );
            storage.create_code_symbol(sym).await.unwrap();
        }
        // Insert a struct symbol
        let sym = CodeSymbol::new(
            "MyStruct".to_string(),
            SymbolType::Struct,
            "src/types.rs".to_string(),
            1,
            20,
            "proj1".to_string(),
        );
        storage.create_code_symbol(sym).await.unwrap();

        // Without filter: should return all 4
        let (all, total) = storage
            .search_symbols("my", None, 10, 0, None, None)
            .await
            .unwrap();
        // Note: MyStruct also matches "my" (case-insensitive)
        assert_eq!(total, 4, "Should find 4 symbols total without filter");
        assert_eq!(all.len(), 4);

        // With symbol_type filter "function": should return 3
        let (funcs, total_funcs) = storage
            .search_symbols("my", None, 10, 0, Some("function"), None)
            .await
            .unwrap();
        assert_eq!(
            total_funcs, 3,
            "Should find 3 function symbols (got total={})",
            total_funcs
        );
        assert_eq!(
            funcs.len(),
            3,
            "search_symbols returned wrong count (got len={})",
            funcs.len()
        );

        // With symbol_type filter "struct": should return 1
        let (structs, total_structs) = storage
            .search_symbols("my", None, 10, 0, Some("struct"), None)
            .await
            .unwrap();
        assert_eq!(total_structs, 1, "Should find 1 struct symbol");
        assert_eq!(structs.len(), 1);
    }

    #[tokio::test]
    async fn test_search_symbols_pagination() {
        let (storage, _tmp) = setup_test_db().await;
        use crate::types::{CodeSymbol, SymbolType};

        // Insert 5 function symbols
        for i in 0..5 {
            let sym = CodeSymbol::new(
                format!("func_{}", i),
                SymbolType::Function,
                format!("src/file_{}.rs", i),
                1,
                10,
                "proj2".to_string(),
            );
            storage.create_code_symbol(sym).await.unwrap();
        }

        // Page 1: limit=2, offset=0
        let (page1, total) = storage
            .search_symbols("func", Some("proj2"), 2, 0, None, None)
            .await
            .unwrap();
        assert_eq!(total, 5, "Total should be 5");
        assert_eq!(page1.len(), 2, "Page 1 should have 2 results");

        // Page 2: limit=2, offset=2
        let (page2, _) = storage
            .search_symbols("func", Some("proj2"), 2, 2, None, None)
            .await
            .unwrap();
        assert_eq!(page2.len(), 2, "Page 2 should have 2 results");

        // Page 3: limit=2, offset=4
        let (page3, _) = storage
            .search_symbols("func", Some("proj2"), 2, 4, None, None)
            .await
            .unwrap();
        assert_eq!(page3.len(), 1, "Page 3 should have 1 result");
    }

    #[tokio::test]
    async fn test_bm25_search_code_no_project_filter() {
        let (storage, _tmp) = setup_test_db().await;
        use crate::types::{ChunkType, CodeChunk, Language};

        // Insert chunks in two different projects
        for i in 0..3 {
            let chunk = CodeChunk {
                id: None,
                file_path: format!("src/alpha_{}.rs", i),
                content: format!("fn alpha_function_{}() {{}}", i),
                language: Language::Rust,
                start_line: 1,
                end_line: 3,
                chunk_type: ChunkType::Function,
                name: Some(format!("alpha_function_{}", i)),
                context_path: None,
                embedding: Some(vec![0.1; 768]),
                content_hash: format!("hash_a_{}", i),
                project_id: Some("project_alpha".to_string()),
                indexed_at: Datetime::default(),
            };
            storage.create_code_chunks_batch(vec![chunk]).await.unwrap();
        }
        for i in 0..2 {
            let chunk = CodeChunk {
                id: None,
                file_path: format!("src/beta_{}.rs", i),
                content: format!("fn alpha_function_{}() {{}}", i),
                language: Language::Rust,
                start_line: 1,
                end_line: 3,
                chunk_type: ChunkType::Function,
                name: Some(format!("alpha_function_{}", i)),
                context_path: None,
                embedding: Some(vec![0.1; 768]),
                content_hash: format!("hash_b_{}", i),
                project_id: Some("project_beta".to_string()),
                indexed_at: Datetime::default(),
            };
            storage.create_code_chunks_batch(vec![chunk]).await.unwrap();
        }

        // Search with project_id = None should find ALL 5 chunks
        let all_results = storage
            .bm25_search_code("alpha_function", None, 100)
            .await
            .unwrap();
        assert_eq!(
            all_results.len(),
            5,
            "bm25_search_code with None project should return all 5 chunks (got {})",
            all_results.len()
        );

        // Search filtered by project
        let alpha_results = storage
            .bm25_search_code("alpha_function", Some("project_alpha"), 100)
            .await
            .unwrap();
        assert_eq!(
            alpha_results.len(),
            3,
            "bm25_search_code filtered by project_alpha should return 3 (got {})",
            alpha_results.len()
        );
    }

    #[tokio::test]
    async fn test_bm25_search_multiword_and_special_chars() {
        let (storage, _tmp) = setup_test_db().await;

        // Store memories with special chars matching what AGENTS.md queries use
        storage
            .create_memory(Memory {
                id: None,
                content: "TASK: WP01-poc-validation\nID: WP01\nStatus: in_progress\nCurrent: T002"
                    .to_string(),
                embedding: Some(vec![0.0; 768]),
                memory_type: MemoryType::Semantic,
                user_id: None,
                metadata: None,
                event_time: Datetime::default(),
                ingestion_time: Datetime::default(),
                valid_from: Datetime::default(),
                valid_until: None,
                importance_score: 1.0,
                invalidation_reason: None,
                content_hash: None,
                embedding_state: Default::default(),
            })
            .await
            .unwrap();

        storage
            .create_memory(Memory {
                id: None,
                content: "PROJECT: memory-mcp\nStatus: active".to_string(),
                embedding: Some(vec![0.0; 768]),
                memory_type: MemoryType::Semantic,
                user_id: None,
                metadata: None,
                event_time: Datetime::default(),
                ingestion_time: Datetime::default(),
                valid_from: Datetime::default(),
                valid_until: None,
                importance_score: 1.0,
                invalidation_reason: None,
                content_hash: None,
                embedding_state: Default::default(),
            })
            .await
            .unwrap();

        // Test multi-word query with special chars (what AGENTS.md protocol searches for)
        let results = storage
            .bm25_search("Status: in_progress", 10)
            .await
            .unwrap();
        assert!(
            !results.is_empty(),
            "search_text('Status: in_progress') should return results"
        );
        assert!(
            results[0].content.contains("Status: in_progress"),
            "top result should preserve phrase relevance"
        );

        // Test prefix search
        let task_results = storage.bm25_search("TASK:", 10).await.unwrap();
        assert_eq!(
            task_results.len(),
            1,
            "search_text('TASK:') should return 1 result (got {})",
            task_results.len()
        );

        // Test word inside content
        let project_results = storage.bm25_search("memory-mcp", 10).await.unwrap();
        assert_eq!(
            project_results.len(),
            1,
            "search_text('memory-mcp') should return 1 result (got {})",
            project_results.len()
        );
    }
}
