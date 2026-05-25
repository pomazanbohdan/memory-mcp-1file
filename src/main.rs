use clap::Parser;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(all(not(target_env = "msvc"), not(target_os = "windows")))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use memory_mcp::codebase::{CodebaseManager, IndexWorker};
use memory_mcp::config::{AppConfig, AppState};
use memory_mcp::embedding::{
    EmbeddingConfig, EmbeddingService, EmbeddingStore, EmbeddingWorker, ModelType,
};
use memory_mcp::search::CodeSearchEngine;
use memory_mcp::server::MemoryMcpServer;
use memory_mcp::storage::{StorageBackend, SurrealStorage};

#[derive(Parser)]
#[command(name = "memory-mcp")]
#[command(about = "MCP memory server for AI agents")]
struct Cli {
    #[arg(long, env, default_value_os_t = default_data_dir())]
    data_dir: PathBuf,

    #[arg(long, env = "EMBEDDING_MODEL", default_value = "e5_multi")]
    model: String,

    #[arg(long, env, default_value = "1000")]
    cache_size: usize,

    #[arg(long, env, default_value = "8")]
    batch_size: usize,

    #[arg(
        long,
        env = "MRL_DIM",
        help = "MRL output dimension (Qwen3/Gemma only). Defaults to model native dim (1024 for qwen3)"
    )]
    mrl_dim: Option<usize>,

    #[arg(long, env = "TIMEOUT_MS", default_value = "30000")]
    timeout: u64,

    /// Maximum time (seconds) a tool call will block waiting for the model to
    /// finish loading. Applies only on the first call on a fresh machine where
    /// the model must be downloaded. Default: 600 s (10 min).
    #[arg(long, env = "MODEL_LOAD_TIMEOUT_SECS", default_value = "600")]
    model_load_timeout_secs: u64,

    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    log_level: String,

    /// Idle timeout in minutes. 0 = disabled (default, recommended for MCP stdio).
    /// Per MCP spec, stdio servers should exit only on stdin close or signals.
    #[arg(long, env, default_value = "0")]
    idle_timeout: u64,

    #[arg(long)]
    list_models: bool,
}

fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("memory-mcp")
}
fn pin_compute_threads() {
    for var in ["RAYON_NUM_THREADS", "MKL_NUM_THREADS", "OMP_NUM_THREADS"] {
        if std::env::var(var).is_err() {
            unsafe { std::env::set_var(var, "2") };
        }
    }
}

fn main() -> anyhow::Result<()> {
    pin_compute_threads();
    let cli = Cli::parse();

    // Candle ML models (especially Qwen3) allocate large tensors on the stack
    // during forward passes. 16 MB covers both SurrealKV WAL recovery and
    // any spawn_blocking inference that inherits the worker stack size.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(16 * 1024 * 1024) // 16 MB
        .build()?;

    runtime.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> anyhow::Result<()> {
    if cli.list_models {
        println!("Available models:");
        println!("  qwen3     - 1024 dim, ~1.2 GB          [Apache 2.0] Top open-source 2026, MRL, 32K ctx");
        println!(
            "  gemma     -  768 dim, ~195 MB           [Gemma license] Lightweight MRL alternative"
        );
        println!(
            "  bge_m3    - 1024 dim, ~420 MB           [MIT] Hybrid dense+sparse+colbert retrieval"
        );
        println!(
            "  nomic     -  768 dim, ~270 MB           [Apache 2.0] Long-context BERT-compatible"
        );
        println!(
            "  e5_multi  -  768 dim, ~180 MB (default) [MIT] Legacy; kept for backward compat"
        );
        println!("  e5_small  -  384 dim,  ~85 MB           [MIT] Minimal RAM, dev/testing only");
        println!();
        println!(
            "NOTE: gemma uses Gemma license (not Apache 2.0). Review terms before commercial use."
        );
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(&cli.log_level)
        .with_writer(std::io::stderr)
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        model = %cli.model,
        data_dir = %cli.data_dir.display(),
        "memory-mcp starting"
    );

    let model: ModelType = cli.model.parse().map_err(|e: String| anyhow::anyhow!(e))?;

    if model.requires_license_agreement() {
        tracing::warn!(
            "LEGAL NOTICE: Model '{}' operates under a proprietary license (not Apache 2.0). \
             Review terms before commercial use.",
            model
        );
    }

    let embedding_config = EmbeddingConfig {
        model,
        cache_size: cli.cache_size,
        batch_size: cli.batch_size,
        mrl_dim: cli.mrl_dim,
        cache_dir: Some(cli.data_dir.join("models")),
    };

    embedding_config
        .validate()
        .map_err(|e| anyhow::anyhow!("Invalid embedding configuration: {}", e))?;

    let storage =
        Arc::new(SurrealStorage::new(&cli.data_dir, embedding_config.output_dim()).await?);

    if let Err(e) = storage.check_dimension(embedding_config.output_dim()).await {
        tracing::warn!("Dimension check: {}", e);
    }

    tracing::info!(output_dim = embedding_config.output_dim(), model = %embedding_config.model, "Embedding engine configured");

    // Initialize Embedding Store (L1/L2 Cache)
    let embedding_store = Arc::new(EmbeddingStore::new(&cli.data_dir, model.repo_id())?);

    let embedding = Arc::new(EmbeddingService::new(embedding_config));
    embedding.start_loading();

    let metrics = std::sync::Arc::new(memory_mcp::embedding::EmbeddingMetrics::new());
    let (queue_tx, queue_rx) = tokio::sync::mpsc::channel(256);
    let adaptive_queue =
        memory_mcp::embedding::AdaptiveEmbeddingQueue::with_defaults(queue_tx, metrics.clone());

    let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);

    let requeue_tx = adaptive_queue.requeue_sender();

    let state = Arc::new(AppState {
        config: AppConfig {
            data_dir: cli.data_dir,
            model: cli.model,
            cache_size: cli.cache_size,
            batch_size: cli.batch_size,
            timeout_ms: cli.timeout,
            log_level: cli.log_level,
            model_load_timeout_ms: cli.model_load_timeout_secs * 1000,
            // New fields: use compile-time defaults (values are documented in AppConfig::default)
            ..AppConfig::default()
        },
        storage: storage.clone(),
        embedding: embedding.clone(),
        embedding_store: embedding_store.clone(),
        embedding_queue: adaptive_queue,
        progress: memory_mcp::config::IndexProgressTracker::new(),
        db_semaphore: Arc::new(tokio::sync::Semaphore::new(10)),
        code_search: Arc::new(CodeSearchEngine::new()),
        indexing_projects: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        shutdown_tx,
        index_pending: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    });

    let worker = EmbeddingWorker::new(
        queue_rx,
        requeue_tx,
        embedding.get_engine(),
        embedding_store.clone(),
        state.clone(),
    );
    tokio::spawn(async move {
        match tokio::spawn(worker.run()).await {
            Ok(count) => tracing::info!(count, "Embedding worker finished"),
            Err(e) => tracing::error!("Embedding worker panicked: {}", e),
        }
    });

    let monitor_state = state.clone();
    let monitor_shutdown = state.shutdown_rx();
    tokio::spawn(memory_mcp::embedding::run_completion_monitor(
        monitor_state,
        monitor_shutdown,
    ));

    // Warm the in-memory BM25 index from existing DB data (background, non-blocking)
    let bm25_state = state.clone();
    tokio::spawn(async move {
        let count = bm25_state
            .code_search
            .load_all_from_storage(bm25_state.storage.as_ref())
            .await;
        if count > 0 {
            tracing::info!(chunks = count, "BM25 in-memory index warmed from DB");
        }
    });

    // Re-embed stale memories (background, non-blocking, throttled)
    let reembed_state = state.clone();
    let reembed_engine = embedding.get_engine();
    tokio::spawn(async move {
        // Wait for embedding engine to be ready
        loop {
            let guard = reembed_engine.read().await;
            if guard.is_some() {
                break;
            }
            drop(guard);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        let stale_memories = match reembed_state.storage.get_stale_memories().await {
            Ok(memories) => memories,
            Err(e) => {
                tracing::warn!("Failed to query stale memories: {}", e);
                return;
            }
        };

        if stale_memories.is_empty() {
            return;
        }

        tracing::info!(count = stale_memories.len(), "Re-embedding stale memories");
        let mut re_embedded = 0u32;
        for memory in &stale_memories {
            let mem_id = match &memory.id {
                Some(thing) => memory_mcp::types::record_key_to_string(&thing.key),
                None => continue,
            };
            let content = memory.content.clone();
            let engine_clone = reembed_engine.clone();
            // Use spawn_blocking to avoid stack overflow — ML model needs large stack
            let emb_result = tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Handle::current();
                let guard = rt.block_on(engine_clone.read());
                match guard.as_ref() {
                    Some(engine) => engine.embed(&content).ok(),
                    None => None,
                }
            })
            .await;

            if let Ok(Some(emb)) = emb_result {
                let hash = blake3::hash(memory.content.as_bytes()).to_hex().to_string();
                if let Err(e) = reembed_state
                    .storage
                    .raw_update_embedding(&mem_id, emb, hash, "current")
                    .await
                {
                    tracing::warn!(id = %mem_id, error = %e, "Failed to update re-embedded memory");
                } else {
                    re_embedded += 1;
                }
            } else {
                tracing::warn!(id = %mem_id, "Failed to re-embed memory");
            }
            // Throttle: 1 memory per second to avoid CPU contention
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        if re_embedded > 0 {
            tracing::info!(count = re_embedded, "Stale memories re-embedded successfully");
        }
    });

    let server = MemoryMcpServer::new(state.clone());

    // ── Lazy-init architecture ─────────────────────────────────────────────
    // `serve_server` is called immediately after lightweight synchronous setup.
    // The MCP `initialize` handshake is handled by `get_info()` which is a
    // pure, synchronous function — it returns in < 1 ms regardless of model
    // state.  The embedding model continues loading in a background OS thread
    // (`start_loading` above).
    //
    // Tool calls that need embeddings use `ensure_embedding_ready!`, which now
    // *waits* up to `model_load_timeout_ms` for the model instead of failing
    // immediately.  This means:
    //   • Fresh machine (model must download):  tool calls block transparently
    //     until the download completes; the MCP session stays alive.
    //   • Warm machine (model cached):  the model is ready in < 5 s; tool
    //     calls proceed with zero perceptible delay.
    //
    // This is the architecturally correct fix for the SIGTERM-on-initialize
    // bug: the server ALWAYS responds to `initialize` instantly; only the
    // heavier tool calls experience startup latency, and only once.
    // ──────────────────────────────────────────────────────────────────────

    // Auto-start codebase manager if /project exists
    let project_path = PathBuf::from("/project");
    if project_path.exists() {
        let project_id = project_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
            .to_string();

        // Create the IndexWorker for this project and start it in the background.
        let (index_worker, index_tx) = IndexWorker::new(state.clone(), project_id.clone());
        // Register the pending-job counter in AppState so the status API can read it.
        state
            .index_pending
            .write()
            .await
            .insert(project_id.clone(), index_tx.pending_arc());
        index_worker.start(state.shutdown_rx());

        // Build the CodebaseManager with the channel sender and start it.
        let mgr = CodebaseManager::new(state.clone(), project_path.clone(), index_tx.clone());
        if let Err(e) = mgr.start().await {
            tracing::error!(error = %e, "Failed to start codebase manager for /project");
        }
        let mgr = Arc::new(mgr);

        // ── Periodic manifest-diff task ─────────────────────────────────
        // Every N minutes, run a lightweight manifest diff and push any
        // discovered changes into the IndexWorker channel.  This catches
        // files that were missed by the file-system watcher (e.g. because
        // the process was restarted or the watcher lost events under heavy load).
        let mgr_for_diff = mgr.clone();
        let mut diff_shutdown = state.shutdown_rx();
        let manifest_diff_interval_mins = state.config.manifest_diff_interval_mins;
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(manifest_diff_interval_mins * 60));
            interval.tick().await; // skip immediate first tick

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        tracing::debug!(
                            project_id = %project_id,
                            "Periodic manifest diff starting"
                        );
                        if let Err(e) = mgr_for_diff.validate_index_full().await {
                            tracing::warn!(
                                project_id = %project_id,
                                error = %e,
                                "Periodic manifest diff failed"
                            );
                        }
                    }
                    _ = diff_shutdown.changed() => {
                        if *diff_shutdown.borrow() {
                            tracing::debug!(project_id = %project_id, "Manifest diff task stopping");
                            break;
                        }
                    }
                }
            }
        });
    }

    let transport = rmcp::transport::io::stdio();

    let service = rmcp::service::serve_server(server, transport).await?;

    if cli.idle_timeout > 0 {
        tracing::warn!(
            minutes = cli.idle_timeout,
            "Non-zero idle timeout is not recommended for MCP stdio transport. \
             Per MCP spec, stdio servers should exit only when stdin is closed or on signals."
        );
    }

    // Periodic storage checkpoint (insurance against SIGKILL from misbehaving clients)
    let checkpoint_storage = state.storage.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(300)); // 5 min
        interval.tick().await; // Skip immediate first tick
        loop {
            interval.tick().await;
            if let Err(e) = checkpoint_storage.shutdown().await {
                tracing::warn!("Periodic checkpoint failed: {}", e);
            } else {
                tracing::debug!("Periodic storage checkpoint completed");
            }
        }
    });

    tracing::info!("Server started, waiting for client disconnect or signals...");

    // Track stdin state for anomaly detection
    let stdin_closed = Arc::new(AtomicBool::new(false));

    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    // MCP stdio lifecycle (spec 2025-03-26 & 2025-11-25):
    //   - Server runs until client closes stdin (service.waiting() resolves)
    //   - Server handles SIGINT/SIGTERM for graceful shutdown
    //   - NO reconnect: stdio is process-level, stdin can't be "reopened"
    //   - Idle timeout is optional and disabled by default
    let idle_future = async {
        if cli.idle_timeout > 0 {
            tokio::time::sleep(Duration::from_secs(cli.idle_timeout * 60)).await;
        } else {
            // Disabled: never resolve
            std::future::pending::<()>().await;
        }
    };

    let shutdown_reason: &str;
    let stdin_closed_flag = stdin_closed.clone();

    tokio::select! {
        res = service.waiting() => {
            stdin_closed_flag.store(true, Ordering::SeqCst);
            match res {
                Ok(_) => {
                    tracing::info!("Client disconnected (stdin closed)");
                    shutdown_reason = "client_disconnect";
                }
                Err(e) => {
                    tracing::error!("Server error: {}", e);
                    shutdown_reason = "server_error";
                }
            }
        },
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutting down gracefully... (SIGINT)");
            shutdown_reason = "sigint";
        },
        _ = async {
            #[cfg(unix)]
            {
                terminate.recv().await;
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await;
            }
        } => {
            let was_stdin_closed = stdin_closed.load(Ordering::SeqCst);
            if !was_stdin_closed {
                tracing::warn!(
                    "SIGTERM received while stdin still open. \
                     Client may have violated MCP spec (expected: stdin close -> SIGTERM). \
                     Possible causes: client timeout, session crash, or external kill. \
                     Allowing 2s grace period for in-flight operations..."
                );
                // Grace period: allow in-flight operations to complete and data to flush
                tokio::time::sleep(Duration::from_secs(2)).await;
            } else {
                tracing::info!("SIGTERM received after stdin closed (normal MCP shutdown)");
            }
            shutdown_reason = "sigterm";
        },
        _ = idle_future => {
            tracing::info!(minutes = cli.idle_timeout, "Idle timeout reached, shutting down");
            shutdown_reason = "idle_timeout";
        }
    }

    tracing::info!(reason = shutdown_reason, "Initiating graceful shutdown...");

    // Signal all background loops to stop
    let _ = state.shutdown_tx.send(true);

    tracing::info!("Flushing database...");
    if let Err(e) = state.storage.shutdown().await {
        tracing::warn!("Database shutdown error: {}", e);
    }

    tracing::info!("Shutdown complete");
    Ok(())
}
