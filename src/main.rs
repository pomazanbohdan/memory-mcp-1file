use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use memory_mcp::config::{AppConfig, AppState, STATUS_ERROR, STATUS_READY};
use memory_mcp::embedding::{
    EmbeddingConfig, EmbeddingService, EmbeddingStore, EmbeddingWorker, ModelType,
};
use memory_mcp::server::MemoryMcpServer;
use memory_mcp::storage::{StorageBackend, SurrealStorage};
use tokio::sync::OnceCell;

#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq)]
pub enum Transport {
    #[default]
    Stdio,
    Http,
}

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

    #[arg(long, env = "TIMEOUT_MS", default_value = "30000")]
    timeout: u64,

    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    log_level: String,

    /// Idle timeout in minutes. Server exits if no requests for this duration. 0 = disabled.
    #[arg(long, env, default_value = "30")]
    idle_timeout: u64,

    /// Reconnect timeout in seconds before shutdown after connection loss.
    #[arg(long, env, default_value = "10")]
    reconnect_timeout: u64,

    #[arg(long)]
    list_models: bool,

    #[arg(long, env = "MCP_TRANSPORT", default_value = "stdio")]
    transport: Transport,

    #[arg(long, env = "MCP_PORT", default_value = "3000")]
    port: u16,

    #[arg(long, env = "MCP_HOST", default_value = "127.0.0.1")]
    host: String,
}

fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("memory-mcp")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.list_models {
        println!("Available models:");
        println!("  e5_small  - 384 dimensions, 134 MB");
        println!("  e5_multi  - 768 dimensions, 1.1 GB (default)");
        println!("  nomic     - 768 dimensions, 1.9 GB");
        println!("  bge_m3    - 1024 dimensions, 2.3 GB");
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(&cli.log_level)
        .with_writer(std::io::stderr)
        .init();

    let model: ModelType = cli.model.parse().map_err(|e: String| anyhow::anyhow!(e))?;

    let embedding_config = EmbeddingConfig {
        model,
        cache_size: cli.cache_size,
        batch_size: cli.batch_size,
        cache_dir: Some(cli.data_dir.join("models")),
    };
    let embedding = Arc::new(EmbeddingService::new(embedding_config));
    embedding.start_loading();

    let metrics = std::sync::Arc::new(memory_mcp::embedding::EmbeddingMetrics::new());
    let (queue_tx, queue_rx) = tokio::sync::mpsc::channel(5000);
    let adaptive_queue =
        memory_mcp::embedding::AdaptiveEmbeddingQueue::with_defaults(queue_tx, metrics.clone());

    let state = Arc::new(AppState {
        config: AppConfig {
            data_dir: cli.data_dir.clone(),
            model: cli.model.clone(),
            cache_size: cli.cache_size,
            batch_size: cli.batch_size,
            timeout_ms: cli.timeout,
            log_level: cli.log_level.clone(),
        },
        storage: Arc::new(OnceCell::const_new()),
        embedding: embedding.clone(),
        embedding_store: Arc::new(OnceCell::const_new()),
        embedding_queue: adaptive_queue,
        progress: memory_mcp::config::IndexProgressTracker::new(),
        db_semaphore: Arc::new(tokio::sync::Semaphore::new(10)),
        init_status: Arc::new(std::sync::atomic::AtomicU8::new(
            memory_mcp::config::STATUS_INITIALIZING,
        )),
        init_error: Arc::new(tokio::sync::RwLock::new(None)),
    });

    let init_state = state.clone();
    let init_data_dir = cli.data_dir.clone();
    let init_model = model;
    tokio::spawn(async move {
        match init_storage(&init_data_dir, init_model, &init_state).await {
            Ok(()) => {
                init_state
                    .init_status
                    .store(STATUS_READY, Ordering::Release);
                tracing::info!("Storage initialization complete");
            }
            Err(e) => {
                let err_msg = e.to_string();
                tracing::error!("Storage initialization failed: {}", err_msg);
                *init_state.init_error.write().await = Some(err_msg);
                init_state
                    .init_status
                    .store(STATUS_ERROR, Ordering::Release);
            }
        }
    });

    let worker = EmbeddingWorker::new(
        queue_rx,
        embedding.get_engine(),
        state.embedding_store.clone(),
        state.clone(),
    );
    tokio::spawn(worker.run());

    let monitor_state = state.clone();
    tokio::spawn(memory_mcp::embedding::run_completion_monitor(monitor_state));

    let server = MemoryMcpServer::new(state.clone());

    match cli.transport {
        Transport::Stdio => {
            run_stdio(server, state.clone(), cli.reconnect_timeout).await?;
        }
        #[cfg(feature = "http")]
        Transport::Http => {
            run_http(state.clone(), &cli.host, cli.port).await?;
        }
        #[cfg(not(feature = "http"))]
        Transport::Http => {
            anyhow::bail!("HTTP transport not enabled. Rebuild with --features http");
        }
    }

    tracing::info!("Initiating graceful shutdown...");

    tracing::info!("Flushing database...");
    if let Some(storage) = state.storage.get() {
        if let Err(e) = storage.shutdown().await {
            tracing::warn!("Database shutdown error: {}", e);
        }
    }

    tracing::info!("Shutdown complete");
    Ok(())
}

#[cfg(feature = "stdio")]
async fn run_stdio(
    server: MemoryMcpServer,
    state: Arc<AppState>,
    reconnect_timeout_sec: u64,
) -> anyhow::Result<()> {
    let transport = rmcp::transport::io::stdio();
    let service = rmcp::service::serve_server(server, transport).await?;

    tracing::info!(
        reconnect_timeout_sec = reconnect_timeout_sec,
        "Server started (stdio), waiting for signals..."
    );

    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let reconnect_timeout = Duration::from_secs(reconnect_timeout_sec);
    let shutdown_reason: &str;

    tokio::select! {
        res = service.waiting() => {
            match res {
                Err(e) => {
                    tracing::error!("Server error: {}", e);
                    shutdown_reason = "server_error";
                }
                Ok(_) => {
                    tracing::info!(
                        timeout_sec = reconnect_timeout_sec,
                        "Connection closed, waiting for reconnect..."
                    );

                    let reconnected = tokio::select! {
                        _ = tokio::time::sleep(reconnect_timeout) => false,
                        _ = tokio::signal::ctrl_c() => {
                            tracing::info!("Received SIGINT during reconnect wait");
                            false
                        }
                    };

                    if reconnected {
                        shutdown_reason = "reconnected";
                    } else {
                        tracing::info!("No reconnect within timeout, shutting down");
                        shutdown_reason = "connection_timeout";
                    }
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
            tracing::info!("Shutting down gracefully... (SIGTERM)");
            shutdown_reason = "sigterm";
        }
    }

    tracing::info!(reason = shutdown_reason, "Stdio transport stopped");
    let _ = state;
    Ok(())
}

#[cfg(feature = "http")]
async fn run_http(state: Arc<AppState>, host: &str, port: u16) -> anyhow::Result<()> {
    use axum::Router;
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    let config = StreamableHttpServerConfig::default();
    let session_manager = Arc::new(LocalSessionManager::default());

    let state_clone = state.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(MemoryMcpServer::new(state_clone.clone())),
        session_manager,
        config,
    );

    let app = Router::new().nest_service("/mcp", mcp_service);

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("MCP HTTP server listening on http://{}/mcp", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to listen for shutdown signal");
            tracing::info!("Received shutdown signal");
        })
        .await?;

    Ok(())
}

async fn init_storage(
    data_dir: &PathBuf,
    model: ModelType,
    state: &AppState,
) -> anyhow::Result<()> {
    let storage = SurrealStorage::new(data_dir).await?;

    if let Err(e) = storage.check_dimension(model.dimensions()).await {
        anyhow::bail!("Dimension mismatch: {}", e);
    }

    let embedding_store = EmbeddingStore::new(data_dir, model.repo_id())?;

    state
        .storage
        .set(storage)
        .map_err(|_| anyhow::anyhow!("Storage already initialized"))?;
    state
        .embedding_store
        .set(embedding_store)
        .map_err(|_| anyhow::anyhow!("EmbeddingStore already initialized"))?;

    Ok(())
}
