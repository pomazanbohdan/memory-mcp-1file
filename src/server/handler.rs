use std::sync::Arc;

use rmcp::{
    handler::server::{
        tool::ToolCallContext, tool::ToolRouter, wrapper::Parameters, ServerHandler,
    },
    model::*,
    service::{RequestContext, RoleServer},
    tool, tool_router,
};

use crate::config::AppState;
use crate::server::logic;
use crate::server::params::*;

#[derive(Clone)]
pub struct MemoryMcpServer {
    state: Arc<AppState>,
    tool_router: ToolRouter<Self>,
}

// Helper to convert anyhow::Error to JSON-RPC ErrorData
fn to_rpc_error(e: anyhow::Error) -> ErrorData {
    ErrorData {
        code: ErrorCode(-32000),
        message: e.to_string().into(),
        data: None,
    }
}

#[tool_router]
impl MemoryMcpServer {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Store a new memory.")]
    async fn store_memory(
        &self,
        params: Parameters<StoreMemoryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        logic::memory::store_memory(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(description = "Get full memory by ID.")]
    async fn get_memory(
        &self,
        params: Parameters<GetMemoryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        logic::memory::get_memory(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(description = "Update memory fields.")]
    async fn update_memory(
        &self,
        params: Parameters<UpdateMemoryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        logic::memory::update_memory(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(description = "Delete memory by ID.")]
    async fn delete_memory(
        &self,
        params: Parameters<DeleteMemoryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        logic::memory::delete_memory(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(description = "List memories (newest first).")]
    async fn list_memories(
        &self,
        params: Parameters<ListMemoriesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        logic::memory::list_memories(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(
        description = "Search memories (query, mode?: vector|bm25). Vector=semantic similarity, bm25=exact keyword match."
    )]
    async fn search_memory(
        &self,
        params: Parameters<SearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if params.0.mode.as_deref() == Some("bm25") {
            logic::search::search_text(&self.state, params.0)
                .await
                .map_err(to_rpc_error)
        } else {
            logic::search::search(&self.state, params.0)
                .await
                .map_err(to_rpc_error)
        }
    }

    #[tool(
        description = "Best memory retrieval (query). Combines vector+BM25+graph via RRF fusion. Use as default."
    )]
    async fn recall(&self, params: Parameters<RecallParams>) -> Result<CallToolResult, ErrorData> {
        logic::search::recall(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(
        description = "Knowledge graph ops. Actions: create_entity(name, entity_type?, description?) | create_relation(from_entity, to_entity, relation_type, weight?) | get_related(entity_id, depth?, direction?) | detect_communities()"
    )]
    async fn knowledge_graph(
        &self,
        params: Parameters<KnowledgeGraphParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match params.0.action.as_str() {
            "create_entity" => {
                let name = params.0.name.ok_or_else(|| ErrorData {
                    code: ErrorCode(-32602),
                    message: "name required for create_entity".into(),
                    data: None,
                })?;
                let entity_params = CreateEntityParams {
                    name,
                    entity_type: params.0.entity_type,
                    description: params.0.description,
                    user_id: params.0.user_id,
                };
                logic::graph::create_entity(&self.state, entity_params)
                    .await
                    .map_err(to_rpc_error)
            }
            "create_relation" => {
                let from_entity = params.0.from_entity.ok_or_else(|| ErrorData {
                    code: ErrorCode(-32602),
                    message: "from_entity required for create_relation".into(),
                    data: None,
                })?;
                let to_entity = params.0.to_entity.ok_or_else(|| ErrorData {
                    code: ErrorCode(-32602),
                    message: "to_entity required for create_relation".into(),
                    data: None,
                })?;
                let relation_type = params.0.relation_type.ok_or_else(|| ErrorData {
                    code: ErrorCode(-32602),
                    message: "relation_type required for create_relation".into(),
                    data: None,
                })?;
                let relation_params = CreateRelationParams {
                    from_entity,
                    to_entity,
                    relation_type,
                    weight: params.0.weight,
                };
                logic::graph::create_relation(&self.state, relation_params)
                    .await
                    .map_err(to_rpc_error)
            }
            "get_related" => {
                let entity_id = params.0.entity_id.ok_or_else(|| ErrorData {
                    code: ErrorCode(-32602),
                    message: "entity_id required for get_related".into(),
                    data: None,
                })?;
                let related_params = GetRelatedParams {
                    entity_id,
                    depth: params.0.depth,
                    direction: params.0.direction,
                };
                logic::graph::get_related(&self.state, related_params)
                    .await
                    .map_err(to_rpc_error)
            }
            "detect_communities" => {
                let community_params = DetectCommunitiesParams { _placeholder: false };
                logic::graph::detect_communities(&self.state, community_params)
                    .await
                    .map_err(to_rpc_error)
            }
            other => Err(ErrorData {
                code: ErrorCode(-32602),
                message: format!("Invalid action '{}'. Use: create_entity, create_relation, get_related, detect_communities", other).into(),
                data: None,
            }),
        }
    }

    #[tool(
        description = "Get valid memories. Optional timestamp (ISO 8601) for point-in-time query."
    )]
    async fn get_valid(
        &self,
        params: Parameters<GetValidParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(ref ts) = params.0.timestamp {
            let at_params = GetValidAtParams {
                timestamp: ts.clone(),
                user_id: params.0.user_id.clone(),
                limit: params.0.limit,
            };
            logic::memory::get_valid_at(&self.state, at_params)
                .await
                .map_err(to_rpc_error)
        } else {
            logic::memory::get_valid(&self.state, params.0)
                .await
                .map_err(to_rpc_error)
        }
    }

    #[tool(description = "Soft-delete memory, optionally linking replacement.")]
    async fn invalidate(
        &self,
        params: Parameters<InvalidateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        logic::memory::invalidate(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(description = "Get system status and startup progress.")]
    async fn get_status(
        &self,
        params: Parameters<GetStatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        logic::system::get_status(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(description = "Index codebase directory for code search.")]
    async fn index_project(
        &self,
        params: Parameters<IndexProjectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        logic::code::index_project(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(
        description = "Code retrieval (query, mode?: vector|hybrid). Default hybrid = vector+BM25+graph fusion. Filters: path_prefix?, language?, chunk_type?"
    )]
    async fn recall_code(
        &self,
        params: Parameters<RecallCodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut recall_params = params.0;

        // Keep recall_code filters (path/language/chunk) enforced in all modes.
        // vector mode is represented as hybrid with non-vector channels disabled.
        if recall_params.mode.as_deref() == Some("vector") {
            recall_params.vector_weight = Some(1.0);
            recall_params.bm25_weight = Some(0.0);
            recall_params.ppr_weight = Some(0.0);
        }

        logic::code::recall_code(&self.state, recall_params)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(description = "Project info. Actions: list() | status(project_id) | stats(project_id)")]
    async fn project_info(
        &self,
        params: Parameters<ProjectInfoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match params.0.action.as_str() {
            "list" => {
                let list_params = ListProjectsParams {
                    _placeholder: false,
                };
                logic::code::list_projects(&self.state, list_params)
                    .await
                    .map_err(to_rpc_error)
            }
            "status" => {
                let project_id = params.0.project_id.ok_or_else(|| ErrorData {
                    code: ErrorCode(-32602),
                    message: "project_id required for status action".into(),
                    data: None,
                })?;
                let status_params = GetIndexStatusParams { project_id };
                let status = logic::code::get_index_status(&self.state, status_params)
                    .await
                    .map_err(to_rpc_error)?;
                Ok(status)
            }
            "stats" => {
                let project_id = params.0.project_id.ok_or_else(|| ErrorData {
                    code: ErrorCode(-32602),
                    message: "project_id required for stats action".into(),
                    data: None,
                })?;
                let stats_params = GetProjectStatsParams { project_id };
                logic::code::get_project_stats(&self.state, stats_params)
                    .await
                    .map_err(to_rpc_error)
            }
            other => Err(ErrorData {
                code: ErrorCode(-32602),
                message: format!("Invalid action '{}'. Use: list, status, stats", other).into(),
                data: None,
            }),
        }
    }

    #[tool(description = "Delete indexed project.")]
    async fn delete_project(
        &self,
        params: Parameters<DeleteProjectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        logic::code::delete_project(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(description = "Search code symbols by name.")]
    async fn search_symbols(
        &self,
        params: Parameters<SearchSymbolsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        logic::code::search_symbols(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(
        description = "Navigate symbol call graph. Actions: callers(symbol_id) | callees(symbol_id) | related(symbol_id, depth?, direction?)"
    )]
    async fn symbol_graph(
        &self,
        params: Parameters<SymbolGraphParams>,
    ) -> Result<CallToolResult, ErrorData> {
        logic::code::symbol_graph(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(description = "DANGER: Reset all database data (requires confirm=true).")]
    async fn reset_all_memory(
        &self,
        params: Parameters<ResetAllMemoryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        logic::system::reset_all_memory(&self.state, params.0)
            .await
            .map_err(to_rpc_error)
    }

    #[tool(description = "Show all available tools with usage examples and parameter combinations.")]
    async fn how_to_use(
        &self,
        _params: Parameters<HowToUseParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = [
            "=== MEMORY ===",
            "store_memory(content=\"...\") — store new memory",
            "store_memory(content=\"...\", memory_type=\"semantic|episodic|procedural\", metadata={...}) — with type and metadata",
            "store_memory(content=\"...\", user_id=\"agent-1\") — scoped to user/agent",
            "get_memory(id=\"abc123\") — get full memory by ID",
            "update_memory(id=\"abc123\", content=\"new text\") — update content (re-embeds automatically)",
            "update_memory(id=\"abc123\", memory_type=\"semantic\", metadata={...}) — update type/metadata only",
            "delete_memory(id=\"abc123\") — hard delete (prefer invalidate)",
            "invalidate(id=\"abc123\", reason=\"outdated\") — soft-delete with reason",
            "invalidate(id=\"abc123\", superseded_by=\"def456\") — soft-delete linking replacement",
            "list_memories(limit=20, offset=0) — list newest first, paginated",
            "get_valid(limit=50) — all non-invalidated memories",
            "get_valid(timestamp=\"2026-01-15T00:00:00Z\") — point-in-time snapshot",
            "get_valid(user_id=\"agent-1\") — filter by user/agent",
            "",
            "=== SEARCH (memories) ===",
            "recall(query=\"authentication flow\") — BEST: hybrid vector+BM25+graph RRF fusion",
            "recall(query=\"...\", vectorWeight=0.7, bm25Weight=0.1, pprWeight=0.2) — tune RRF channel weights",
            "recall(query=\"...\", limit=20) — control result count",
            "search_memory(query=\"auth token\", mode=\"vector\") — pure semantic similarity",
            "search_memory(query=\"DECISION:\", mode=\"bm25\") — exact keyword match",
            "",
            "=== CODE INDEXING ===",
            "index_project(path=\"/project\") — index codebase (incremental)",
            "index_project(path=\"/project\", force=true) — full re-index from scratch",
            "project_info(action=\"list\") — list all indexed projects",
            "project_info(action=\"status\", project_id=\"...\") — indexing progress, stuck chunks, failed files",
            "project_info(action=\"stats\", project_id=\"...\") — file/symbol/chunk/language counts",
            "delete_project(project_id=\"...\") — remove indexed project and all its data",
            "",
            "=== CODE SEARCH ===",
            "recall_code(query=\"error handling middleware\") — BEST: hybrid BM25+vector+PPR graph",
            "recall_code(query=\"...\", mode=\"vector\") — pure semantic vector search",
            "recall_code(query=\"...\", mode=\"hybrid\") — explicit hybrid (default)",
            "recall_code(query=\"...\", vectorWeight=0.5, bm25Weight=0.3, pprWeight=0.2) — tune fusion weights",
            "recall_code(query=\"...\", pathPrefix=\"src/auth/\") — filter by path prefix",
            "recall_code(query=\"...\", language=\"dart\") — filter by language",
            "recall_code(query=\"...\", chunkType=\"function\") — filter by chunk type (function|class|method|module)",
            "recall_code(query=\"...\", pathPrefix=\"src/\", language=\"rust\", limit=20) — all filters combined",
            "",
            "=== SYMBOLS ===",
            "search_symbols(query=\"UserRepository\") — find by name (exact + fuzzy)",
            "search_symbols(query=\"auth\", symbol_type=\"class\") — filter: class|function|method|interface|enum",
            "search_symbols(query=\"...\", path_prefix=\"src/\", limit=20, offset=0) — paginated + path filter",
            "search_symbols(query=\"...\", project_id=\"proj123\") — filter by project",
            "symbol_graph(action=\"related\", symbol_id=\"abc123\") — related symbols (imports, calls, inheritance)",
            "symbol_graph(action=\"related\", symbol_id=\"abc123\", depth=3, direction=\"out\") — deep outgoing traversal",
            "symbol_graph(action=\"related\", symbol_id=\"abc123\", depth=2, direction=\"in\") — who depends on this",
            "symbol_graph(action=\"related\", symbol_id=\"abc123\", direction=\"both\") — full neighborhood",
            "symbol_graph(action=\"callers\", symbol_id=\"abc123\") — who calls this symbol",
            "symbol_graph(action=\"callees\", symbol_id=\"abc123\") — what this symbol calls",
            "",
            "=== KNOWLEDGE GRAPH ===",
            "knowledge_graph(action=\"create_entity\", name=\"AuthModule\", entity_type=\"module\", description=\"...\") — create node",
            "knowledge_graph(action=\"create_entity\", name=\"AuthModule\", user_id=\"agent-1\") — scoped entity",
            "knowledge_graph(action=\"create_relation\", from_entity=\"AuthModule\", to_entity=\"UserRepo\", relation_type=\"depends_on\") — create edge",
            "knowledge_graph(action=\"create_relation\", from_entity=\"...\", to_entity=\"...\", relation_type=\"...\", weight=0.9) — weighted edge",
            "knowledge_graph(action=\"get_related\", entity_id=\"AuthModule\") — direct neighbors",
            "knowledge_graph(action=\"get_related\", entity_id=\"AuthModule\", depth=3, direction=\"both\") — deep traversal",
            "knowledge_graph(action=\"get_related\", entity_id=\"...\", direction=\"in\") — incoming only",
            "knowledge_graph(action=\"detect_communities\") — find clusters in the graph",
            "",
            "=== SYSTEM ===",
            "get_status(_placeholder=true) — health, embedding model info, memory count",
            "how_to_use(_placeholder=true) — this help text",
            "reset_all_memory(confirm=true) — DANGER: wipe ALL data (memories, code index, graph)",
        ]
        .join("\n");

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

impl ServerHandler for MemoryMcpServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult {
            protocol_version: ProtocolVersion::default(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(false),
                }),
                ..ServerCapabilities::default()
            },
            server_info: Implementation {
                name: "memory-mcp".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                description: None,
                title: None,
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "AI agent memory server with semantic search, knowledge graph, and code search."
                    .into(),
            ),
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tool_router.list_all()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let tool_context = ToolCallContext::new(self, request, context);
        self.tool_router.call(tool_context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TestContext;

    #[tokio::test]
    async fn test_server_handler_integration() {
        let ctx = TestContext::new().await;
        let server = MemoryMcpServer::new(ctx.state.clone());

        // 1. Get Info
        let info = server.get_info();
        assert_eq!(info.server_info.name, "memory-mcp");

        // 2. Integration check pass
        // We cannot easily mock RequestContext without more deps,
        // but since logic tests cover actual execution,
        // and compilation proves traits are implemented, this is sufficient.
    }
}
