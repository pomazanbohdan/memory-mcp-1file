//! Storage backend trait definition
//!
//! Defines the async interface for all storage operations.
//! Implemented by SurrealStorage.

use crate::types::Datetime;
use std::collections::HashMap;

use crate::types::{
    CodeChunk, CodeSymbol, Direction, Entity, IndexStatus, ManifestEntry, Memory, MemoryUpdate,
    Relation, ScoredCodeChunk, SearchResult, SymbolRelation,
};
use crate::Result;

/// Storage backend trait for all database operations
#[allow(async_fn_in_trait)]
pub trait StorageBackend: Send + Sync {
    // ─────────────────────────────────────────────────────────────────────────
    // Memory CRUD
    // ─────────────────────────────────────────────────────────────────────────

    /// Store a new memory, returns the generated ID
    async fn create_memory(&self, memory: Memory) -> Result<String>;

    /// Get a memory by ID
    async fn get_memory(&self, id: &str) -> Result<Option<Memory>>;

    /// Update an existing memory
    async fn update_memory(&self, id: &str, update: MemoryUpdate) -> Result<Memory>;

    /// Delete a memory by ID, returns true if deleted
    async fn delete_memory(&self, id: &str) -> Result<bool>;

    /// List memories with pagination, sorted by ingestion_time DESC
    async fn list_memories(&self, limit: usize, offset: usize) -> Result<Vec<Memory>>;

    /// Count total number of memories
    async fn count_memories(&self) -> Result<usize>;

    // ─────────────────────────────────────────────────────────────────────────
    // Vector search
    // ─────────────────────────────────────────────────────────────────────────

    /// Vector similarity search on memories
    async fn vector_search(&self, embedding: &[f32], limit: usize) -> Result<Vec<SearchResult>>;

    /// Vector similarity search on code chunks
    async fn vector_search_code(
        &self,
        embedding: &[f32],
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ScoredCodeChunk>>;

    /// Vector similarity search on code symbols (for graph-based recall_code)
    async fn vector_search_symbols(
        &self,
        embedding: &[f32],
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodeSymbol>>;

    // ─────────────────────────────────────────────────────────────────────────
    // BM25 search
    // ─────────────────────────────────────────────────────────────────────────

    /// Full-text BM25 search on memories
    async fn bm25_search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>>;

    /// Text relevance search on code chunks
    async fn bm25_search_code(
        &self,
        query: &str,
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ScoredCodeChunk>>;

    // ─────────────────────────────────────────────────────────────────────────
    // Entity operations
    // ─────────────────────────────────────────────────────────────────────────

    /// Create a new entity, returns the generated ID
    async fn create_entity(&self, entity: Entity) -> Result<String>;

    /// Get an entity by ID
    async fn get_entity(&self, id: &str) -> Result<Option<Entity>>;

    /// Search entities by name using BM25
    async fn search_entities(&self, query: &str, limit: usize) -> Result<Vec<Entity>>;

    // ─────────────────────────────────────────────────────────────────────────
    // Relation operations
    // ─────────────────────────────────────────────────────────────────────────

    /// Create a relation between two entities, returns the relation ID
    async fn create_relation(&self, relation: Relation) -> Result<String>;

    /// Get related entities via graph traversal
    async fn get_related(
        &self,
        entity_id: &str,
        depth: usize,
        direction: Direction,
    ) -> Result<(Vec<Entity>, Vec<Relation>)>;

    /// Get subgraph containing specified entities and their relations
    async fn get_subgraph(&self, entity_ids: &[String]) -> Result<(Vec<Entity>, Vec<Relation>)>;

    /// Get the degree (number of connections) for each entity
    async fn get_node_degrees(&self, entity_ids: &[String]) -> Result<HashMap<String, usize>>;

    /// Get all entities in the graph
    async fn get_all_entities(&self) -> Result<Vec<Entity>>;

    /// Get all relations in the graph
    async fn get_all_relations(&self) -> Result<Vec<Relation>>;

    // ─────────────────────────────────────────────────────────────────────────
    // Temporal operations
    // ─────────────────────────────────────────────────────────────────────────

    /// Get currently valid memories (valid_until is None or in the future)
    async fn get_valid(&self, user_id: Option<&str>, limit: usize) -> Result<Vec<Memory>>;

    /// Get memories that were valid at a specific point in time
    async fn get_valid_at(
        &self,
        timestamp: Datetime,
        user_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Memory>>;

    /// Invalidate a memory (soft delete by setting valid_until)
    async fn invalidate(
        &self,
        id: &str,
        reason: Option<&str>,
        superseded_by: Option<&str>,
    ) -> Result<bool>;

    // ─────────────────────────────────────────────────────────────────────────
    // Code operations
    // ─────────────────────────────────────────────────────────────────────────

    /// Create a single code chunk, returns the generated ID
    async fn create_code_chunk(&self, chunk: CodeChunk) -> Result<String>;

    /// Create code chunks in batch, returns (id, chunk) pairs to avoid caller cloning
    async fn create_code_chunks_batch(
        &self,
        chunks: Vec<CodeChunk>,
    ) -> Result<Vec<(String, CodeChunk)>>;

    /// Delete all code chunks for a project, returns count of deleted chunks
    async fn delete_project_chunks(&self, project_id: &str) -> Result<usize>;

    /// Delete all chunks for a specific file path within a project
    async fn delete_chunks_by_path(&self, project_id: &str, file_path: &str) -> Result<usize>;

    /// Get all chunks for a specific file path within a project
    async fn get_chunks_by_path(&self, project_id: &str, file_path: &str)
        -> Result<Vec<CodeChunk>>;

    /// Get all code chunks for a project (used to build in-memory BM25 index)
    async fn get_all_chunks_for_project(&self, project_id: &str) -> Result<Vec<CodeChunk>>;

    /// Paginated code-chunk fetch used by the streaming BM25 warm-up.
    /// Returns up to `limit` chunks starting from row `offset` (zero-based).
    /// When the returned `Vec` is shorter than `limit` (or empty) the caller
    /// knows it has reached the last page.
    async fn get_chunks_paginated(
        &self,
        project_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<CodeChunk>>;

    /// Fetch specific code chunks by their string IDs (e.g. "abc123").
    /// Used by the BM25 search to hydrate content for top-N results without
    /// keeping all chunk content in RAM.
    async fn get_chunks_by_ids(&self, ids: &[String]) -> Result<Vec<CodeChunk>>;

    /// Clear all embeddings for a project, forcing re-embedding via resume pipeline.
    /// Sets embedding = NONE on all chunks and symbols for the given project.
    async fn clear_project_embeddings(&self, project_id: &str) -> Result<u64>;

    /// Get indexing status for a project
    async fn get_index_status(&self, project_id: &str) -> Result<Option<IndexStatus>>;

    /// Update/upsert indexing status for a project
    async fn update_index_status(&self, status: IndexStatus) -> Result<()>;

    /// Delete indexing status for a project
    async fn delete_index_status(&self, project_id: &str) -> Result<()>;

    /// List all indexed project IDs
    async fn list_projects(&self) -> Result<Vec<String>>;

    // ─────────────────────────────────────────────────────────────────────────
    // File hash operations (incremental indexing)
    // ─────────────────────────────────────────────────────────────────────────

    /// Get stored file hash for incremental index comparison
    async fn get_file_hash(&self, project_id: &str, file_path: &str) -> Result<Option<String>>;

    /// Set/update file hash after indexing
    async fn set_file_hash(&self, project_id: &str, file_path: &str, hash: &str) -> Result<()>;

    /// Batch set/update file hashes after indexing (single DB round-trip)
    async fn set_file_hashes_batch(
        &self,
        project_id: &str,
        hashes: &[(String, String)],
    ) -> Result<()>;

    /// Delete all file hashes for a project (used during full re-index)
    async fn delete_file_hashes(&self, project_id: &str) -> Result<()>;

    /// Delete file hash for a specific file (used when file is deleted)
    async fn delete_file_hash(&self, project_id: &str, file_path: &str) -> Result<()>;

    // ─────────────────────────────────────────────────────────────────────────
    // Code Graph operations
    // ─────────────────────────────────────────────────────────────────────────

    /// Create a single code symbol
    async fn create_code_symbol(&self, symbol: CodeSymbol) -> Result<String>;

    /// Create multiple code symbols in a batch, returns IDs of created symbols
    async fn create_code_symbols_batch(&self, symbols: Vec<CodeSymbol>) -> Result<Vec<String>>;

    async fn update_symbol_embedding(&self, id: &str, embedding: Vec<f32>) -> Result<()>;

    async fn update_chunk_embedding(&self, id: &str, embedding: Vec<f32>) -> Result<()>;

    /// Batch update symbol embeddings - more efficient than individual updates
    async fn batch_update_symbol_embeddings(&self, updates: &[(String, Vec<f32>)]) -> Result<()>;

    /// Batch update chunk embeddings - more efficient than individual updates
    async fn batch_update_chunk_embeddings(&self, updates: &[(String, Vec<f32>)]) -> Result<()>;

    /// Create a relation between code symbols
    async fn create_symbol_relation(&self, relation: SymbolRelation) -> Result<String>;

    /// Batch-create symbol relations in a single query instead of N individual RELATE calls
    async fn create_symbol_relations_batch(&self, relations: Vec<SymbolRelation>) -> Result<u32>;

    /// Delete all symbols for a project
    async fn delete_project_symbols(&self, project_id: &str) -> Result<usize>;

    /// Delete all symbols for a specific file
    async fn delete_symbols_by_path(&self, project_id: &str, file_path: &str) -> Result<usize>;

    /// Get all symbols for a project (for building cross-file SymbolIndex)
    async fn get_project_symbols(&self, project_id: &str) -> Result<Vec<CodeSymbol>>;

    /// Find all symbols that call a given symbol
    async fn get_symbol_callers(&self, symbol_id: &str) -> Result<Vec<CodeSymbol>>;

    /// Find all symbols called by a given symbol
    async fn get_symbol_callees(&self, symbol_id: &str) -> Result<Vec<CodeSymbol>>;

    /// Get related symbols via graph traversal
    async fn get_related_symbols(
        &self,
        symbol_id: &str,
        depth: usize,
        direction: Direction,
    ) -> Result<(Vec<CodeSymbol>, Vec<SymbolRelation>)>;

    /// Get code subgraph for a set of symbol IDs (for recall_code PageRank)
    async fn get_code_subgraph(
        &self,
        symbol_ids: &[String],
    ) -> Result<(Vec<CodeSymbol>, Vec<SymbolRelation>)>;

    /// Search symbols by name pattern
    async fn search_symbols(
        &self,
        query: &str,
        project_id: Option<&str>,
        limit: usize,
        offset: usize,
        symbol_type: Option<&str>,
        path_prefix: Option<&str>,
    ) -> Result<(Vec<CodeSymbol>, u32)>;

    /// Replace symbol->chunk overlap mappings for a project.
    ///
    /// Input rows: (symbol_id, chunk_id, overlap_score), where IDs are plain
    /// record keys without table prefix.
    async fn replace_symbol_chunk_map(
        &self,
        project_id: &str,
        rows: &[(String, String, f32)],
    ) -> Result<u32>;

    /// Retrieve mapped chunk IDs for a list of symbol IDs (record keys).
    /// Returns (chunk_id, best_overlap_score), sorted by overlap descending.
    async fn get_mapped_chunks_for_symbols(
        &self,
        project_id: &str,
        symbol_ids: &[String],
        limit: usize,
    ) -> Result<Vec<(String, f32)>>;

    // ─────────────────────────────────────────────────────────────────────────
    // Statistics & Counts
    // ─────────────────────────────────────────────────────────────────────────

    /// Count total symbols for a project
    async fn count_symbols(&self, project_id: &str) -> Result<u32>;

    /// Count total chunks for a project
    async fn count_chunks(&self, project_id: &str) -> Result<u32>;

    /// Count symbols that have embeddings (embedding IS NOT NULL)
    async fn count_embedded_symbols(&self, project_id: &str) -> Result<u32>;

    /// Count chunks that have embeddings (embedding IS NOT NULL)
    async fn count_embedded_chunks(&self, project_id: &str) -> Result<u32>;

    /// Get chunks that have no embedding yet (for resume after interruption)
    async fn get_unembedded_chunks(&self, project_id: &str) -> Result<Vec<(String, String)>>;

    /// Get symbols that have no embedding yet (for resume after interruption)
    async fn get_unembedded_symbols(&self, project_id: &str) -> Result<Vec<(String, String)>>;

    /// Count symbol relations for a project (useful for debugging graph)
    async fn count_symbol_relations(&self, project_id: &str) -> Result<u32>;

    /// Find a symbol by name across the project (for cross-file resolution)
    async fn find_symbol_by_name(
        &self,
        project_id: &str,
        name: &str,
    ) -> Result<Option<crate::types::symbol::CodeSymbol>>;

    /// Batch-find symbols by names across the project (single DB round-trip).
    async fn find_symbols_by_names(
        &self,
        project_id: &str,
        names: &[String],
    ) -> Result<Vec<CodeSymbol>>;

    /// Find symbol by name with file preference for better resolution
    async fn find_symbol_by_name_with_context(
        &self,
        project_id: &str,
        name: &str,
        prefer_file: Option<&str>,
    ) -> Result<Option<CodeSymbol>>;

    // ─────────────────────────────────────────────────────────────────────────
    // System
    // ─────────────────────────────────────────────────────────────────────────

    /// Check if the database is healthy and responsive
    async fn health_check(&self) -> Result<bool>;

    /// Reset the entire database (delete all data). DANGER.
    async fn reset_db(&self) -> Result<()>;

    /// Gracefully shutdown the database, flushing any pending writes
    async fn shutdown(&self) -> Result<()>;

    // ─────────────────────────────────────────────────────────────────────────
    // File manifest operations (track all files for deletion detection)
    // ─────────────────────────────────────────────────────────────────────────

    /// Upsert a file path entry in the manifest (mark as seen now).
    async fn upsert_manifest_entry(&self, project_id: &str, file_path: &str) -> Result<()>;

    /// Batch upsert multiple file paths in the manifest (single round-trip).
    async fn upsert_manifest_entries(&self, project_id: &str, file_paths: &[String]) -> Result<()>;

    /// Get all file paths currently tracked in the manifest for a project.
    async fn get_manifest_entries(&self, project_id: &str) -> Result<Vec<ManifestEntry>>;

    /// Delete all manifest entries for a project (used on full re-index).
    async fn delete_manifest_entries(&self, project_id: &str) -> Result<()>;

    /// Delete a single manifest entry (file was removed from project).
    async fn delete_manifest_entry(&self, project_id: &str, file_path: &str) -> Result<()>;

    /// Count the number of files tracked in the manifest for a project.
    /// More efficient than `get_manifest_entries(...).len()` as it avoids
    /// loading all entries into memory.
    async fn count_manifest_entries(&self, project_id: &str) -> Result<usize>;
}
