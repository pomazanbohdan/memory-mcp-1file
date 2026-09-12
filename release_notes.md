## Unreleased

No changes recorded.

## Release v0.9.1: Windows npm packaging & Codex

This patch release fixes the Windows npm installation path and adds a
project-scoped OpenAI Codex configuration.

### What's Fixed:
* **Windows npm packaging:** Windows release ZIPs now contain
  `memory-mcp.exe` at the archive root.
* **Defensive installer normalization:** The npm installer accepts legacy
  nested archives, rejects symlinks and duplicate binaries, and copies the
  validated executable with exclusive-create semantics.
* **OpenAI Codex integration:** Trusted repositories can use
  `.codex/config.toml` with a project 16-tool allowlist and project-local data.
* **Release verification:** npm installer syntax, regression tests, and the
  Windows archive root layout are checked in CI.

---

## Release v0.9.0: MCP Protocol, Runtime & Granite

This release updates MCP lifecycle handling, the embedding runtime, and code-search
stability for production clients.

### What's New:
* **Stateless MCP 2026-07-28:** Modern clients can start with the standard
  `server/discover` request and send protocol/client metadata in each request's
  `_meta`. The server keeps no MCP session, `Mcp-Session-Id`, or mutable
  `currentProject` state.
* **Legacy compatibility:** The 2025-11-25 `initialize`/`initialized` flow remains
  supported for existing clients.
* **Transport contour:** This binary exposes MCP over stdio for one local
  workspace per process. Streamable HTTP is not enabled or advertised by this
  release; no HTTP session state is introduced.
* **Workspace context:** `index_project` accepts an explicit path, and code
  retrieval/search tools accept `project_id` filters when a process indexes more
  than one project. Roots negotiation is not used.
* **Embedding runtime:** Updated Candle Transformers to 0.11. Fresh installations
  use Granite as the default 384-dimensional model; existing `e5_multi` data
  remains available with explicit model selection.
* **Granite embeddings:** Added
  `ibm-granite/granite-embedding-97m-multilingual-r2` with Candle ModernBERT
  inference, SiLU activation, CLS pooling, and 384-dimensional Apache 2.0
  embeddings.
* **Code-search stability:** Storage text search no longer depends on the legacy
  code full-text index that could prevent existing databases from opening.

---

## Release v0.8.2: Security Hardening & Model Updates 🔒

This release focuses on critical security fixes and model configuration updates.

### What's New:
* **Model Migration (Gemma → e5_multi):** Updated the default embedding model from `gemma` to `e5_multi` (`intfloat/multilingual-e5-base`). This change addresses supply chain concerns while maintaining excellent retrieval performance.
* **Security Hardening:** Multiple critical security vulnerabilities have been addressed (see PR list for details).
* **Mimalloc Allocator:** Replaced the system allocator with `mimalloc`. This drastically reduces memory fragmentation (especially on Alpine/Musl) and significantly boosts multi-threaded processing speeds.
* **SurrealDB Stability (Throttling):** We implemented smart batch-throttling during indexation. The indexer now pauses for 100-150ms after inserting vectors, completely eliminating `Transaction write conflict` (OCC Retries) inside SurrealDB.
* **768d Vectors:** The model natively generates and searches against 768-dimensional vectors with `last_token_pooling` for immense accuracy. The database schema dynamically rebuilds its `HNSW` indices to accommodate the new dimension.
* **Hardware Acceleration:** Native release builds now enable `x86-64-v3` target optimizations, speeding up the underlying tensor math via AVX2.
* **Cleanup:** Removed the broken `accelerate` feature from Cargo to ensure proper compilation on Linux.

### Performance:
On a standard system, the container now sits comfortably at **~350MB of RAM usage** (down from ~4GB!) during massive codebase indexation, keeping your system fast and responsive.
