# memory-mcp-1file

MCP memory server with semantic search, code indexing, and knowledge graph for AI agents.

## Quick Start

```bash
# Run directly (downloads binary automatically)
npx memory-mcp-1file

# Or with bun
bunx memory-mcp-1file
```
Windows installations from `0.9.1` onward normalize the release archive into
`bin/memory-mcp.exe`, including archives with a nested target directory.
Starting with `0.9.2`, the launcher also handles npm/npx forwarding its
option separator before binary arguments.

For OpenAI Codex, use the repository's trusted-project `.codex/config.toml`,
which pins `memory-mcp-1file@0.9.2` and stores the database under
`.codex/data`.


## What is this?

`memory-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io/) server that provides AI agents with:

- **Semantic memory** — store and search memories with embeddings
- **Code indexing** — parse and index codebases with tree-sitter
- **Knowledge graph** — entity extraction and relationship tracking
- **Temporal awareness** — time-based memory queries

Use with Claude Code, OpenAI Codex, Cursor, or any MCP-compatible client:

```json
{
  "mcpServers": {
    "memory": {
      "command": "npx",
      "args": ["-y", "memory-mcp-1file"]
    }
  }
}
```

## CLI Options

```bash
memory-mcp --help          # Show all options
memory-mcp --data-dir /data # Custom database path
```

## Supported Platforms

| Platform | Architecture |
|---|---|
| Linux | x86_64 (musl) |
| macOS | x86_64, ARM64 (Apple Silicon) |
| Windows | x86_64 |

## Links

- [GitHub Repository](https://github.com/pomazanbohdan/memory-mcp-1file)
- [Releases](https://github.com/pomazanbohdan/memory-mcp-1file/releases)
- [Architecture](https://github.com/pomazanbohdan/memory-mcp-1file/blob/master/ARCHITECTURE.md)

## License

MIT
