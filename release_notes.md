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
