# ADR-002: Streamable HTTP Transport for MCP

**Status:** Proposed  
**Date:** 2026-01-12  
**Context:** OpenCode stdio transport issues, need for HTTP-based alternative

## Problem

MCP clients experience `connection closed` / `Failed to get tools` errors with stdio transport due to:
1. Client-side stdio handling issues (race conditions, subprocess communication)
2. Strict `initialized` notification timing requirements
3. No easy way to debug transport issues

Stdio works correctly when tested manually but fails in some MCP clients (OpenCode, others).

```
Current: Only stdio transport
         └── Works: Claude Desktop, manual testing
         └── Fails: OpenCode (intermittently)
```

## Decision

Add **Streamable HTTP transport** as an alternative to stdio, following MCP 2025-03-26 specification.

### Transport Selection

```
CLI:  memory-mcp --transport stdio|http [--port 3000] [--host 127.0.0.1]
ENV:  MCP_TRANSPORT=http MCP_PORT=3000 MCP_HOST=127.0.0.1

Priority: CLI args > Environment variables > Defaults
Default:  stdio (backwards compatible)
```

## Solution Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ DUAL TRANSPORT ARCHITECTURE                                     │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌──────────────────┐                                          │
│  │  MemoryMcpServer │  ← Transport-agnostic (unchanged)        │
│  │  (handler.rs)    │                                          │
│  └────────┬─────────┘                                          │
│           │                                                     │
│     ┌─────┴─────┐                                              │
│     ▼           ▼                                              │
│  ┌──────┐   ┌──────────────────┐                               │
│  │ STDIO│   │ STREAMABLE HTTP  │                               │
│  │      │   │                  │                               │
│  │stdin │   │ POST /mcp        │  ← Client requests            │
│  │stdout│   │ GET  /mcp        │  ← SSE stream                 │
│  └──────┘   │ Mcp-Session-Id   │  ← Session management         │
│             └──────────────────┘                               │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Implementation

### 1. Cargo.toml Changes

```toml
[dependencies]
axum = { version = "0.8", optional = true }
tokio = { version = "1", features = ["net"] }  # Add "net" feature

[features]
default = ["stdio", "http"]
stdio = ["rmcp/transport-io"]
http = ["rmcp/transport-streamable-http-server", "dep:axum"]
```

### 2. CLI Arguments (main.rs)

```rust
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum Transport {
    #[default]
    Stdio,
    Http,
}

#[derive(Parser)]
struct Cli {
    // ... existing args ...
    
    /// Transport type: stdio (default) or http
    #[arg(long, env = "MCP_TRANSPORT", default_value = "stdio")]
    transport: Transport,
    
    /// HTTP server port (only for http transport)
    #[arg(long, env = "MCP_PORT", default_value = "3000")]
    port: u16,
    
    /// HTTP server host binding (only for http transport)
    #[arg(long, env = "MCP_HOST", default_value = "127.0.0.1")]
    host: String,
}
```

### 3. Transport Runner (main.rs)

```rust
async fn main() -> anyhow::Result<()> {
    // ... existing initialization ...
    
    let server = MemoryMcpServer::new(state.clone());
    
    match cli.transport {
        Transport::Stdio => run_stdio(server, state).await,
        Transport::Http => run_http(server, state, &cli.host, cli.port).await,
    }
}

async fn run_stdio(server: MemoryMcpServer, state: Arc<AppState>) -> anyhow::Result<()> {
    let transport = rmcp::transport::io::stdio();
    let service = rmcp::service::serve_server(server, transport).await?;
    
    tokio::select! {
        _ = service.waiting() => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received shutdown signal");
        }
    }
    
    // ... graceful shutdown ...
    Ok(())
}

#[cfg(feature = "http")]
async fn run_http(
    server: MemoryMcpServer,
    state: Arc<AppState>,
    host: &str,
    port: u16,
) -> anyhow::Result<()> {
    use axum::Router;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, StreamableHttpServerConfig, session::local::LocalSessionManager,
    };
    
    let config = StreamableHttpServerConfig::default();
    let session_manager = LocalSessionManager::default();
    
    let mcp_service = StreamableHttpService::new(
        move || MemoryMcpServer::new(state.clone()),
        session_manager,
        config,
    );
    
    let app = Router::new().nest_service("/mcp", mcp_service);
    
    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("MCP HTTP server listening on http://{}/mcp", addr);
    
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    
    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.expect("Failed to listen for shutdown signal");
    tracing::info!("Received shutdown signal");
}
```

### 4. Server Factory Pattern

For HTTP transport, `StreamableHttpService` needs a factory function to create new server instances per session:

```rust
// Option A: Clone state, create new server per session
let state_clone = state.clone();
StreamableHttpService::new(
    move || MemoryMcpServer::new(state_clone.clone()),
    session_manager,
    config,
)

// This works because:
// - AppState uses Arc<> for all shared data
// - MemoryMcpServer is cheap to construct
// - Storage/EmbeddingStore are shared via OnceCell
```

## Usage Examples

### Stdio (Default, Backwards Compatible)

```bash
# All equivalent:
memory-mcp --data-dir /data
memory-mcp --transport stdio --data-dir /data
MCP_TRANSPORT=stdio memory-mcp --data-dir /data
```

### HTTP Transport

```bash
# CLI
memory-mcp --transport http --port 3000 --host 127.0.0.1

# Environment
MCP_TRANSPORT=http MCP_PORT=8080 memory-mcp

# Docker
docker run --rm -p 3000:3000 \
  -e MCP_TRANSPORT=http \
  -v mcp-data:/data \
  memory-mcp:latest
```

### Client Configuration (HTTP)

```json
{
  "mcpServers": {
    "memory": {
      "type": "streamable-http",
      "url": "http://localhost:3000/mcp"
    }
  }
}
```

## Security Considerations

1. **Default to localhost**: `--host 127.0.0.1` prevents external access
2. **No auth by default**: For local development only
3. **Origin validation**: rmcp validates Origin header
4. **Future**: Add `--auth-token` for production deployments

## Alternatives Considered

| # | Alternative | Why Not Chosen |
|---|-------------|----------------|
| 1 | Separate binary | Code duplication, harder maintenance |
| 2 | Compile-time features only | Less flexible, need multiple builds |
| 3 | Auto-detect transport | Magic behavior, hard to debug |
| 4 | Subcommands | Breaking change to CLI interface |
| 5 | HTTP-only | Breaks existing stdio users |

## Consequences

### Positive
- Fixes OpenCode connection issues
- Enables remote/cloud deployments
- Easier debugging (HTTP is inspectable)
- Horizontal scaling possible with HTTP
- Backwards compatible (stdio default)

### Negative
- Larger binary size (axum dependency)
- More configuration options
- HTTP has slightly higher latency than stdio

### Neutral
- Docker config changes needed for HTTP mode
- Documentation updates required

## Migration Path

1. **Phase 1**: Add HTTP transport, keep stdio default
2. **Phase 2**: Test with OpenCode, collect feedback
3. **Phase 3**: Consider making HTTP default for Docker

## References

- [MCP Specification: Transports (2025-03-26)](https://modelcontextprotocol.io/specification/2025-03-26/basic/transports)
- [rmcp StreamableHttpService](https://github.com/modelcontextprotocol/rust-sdk)
- [ADR-001: Lazy Initialization](./ADR-001-lazy-initialization.md)
