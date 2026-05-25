use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

use super::cache::EmbeddingCache;
use super::cleanup::{cleanup_model_cache, CleanupConfig};
use super::config::{EmbeddingConfig, ModelType};
use super::engine::EmbeddingEngine;
use super::{EmbeddingStatus, LoadingPhase};
use crate::types::{AppError, Result};

const STATUS_LOADING: u8 = 0;
const STATUS_READY: u8 = 1;
const STATUS_ERROR: u8 = 2;
const REDACTED_LOAD_ERROR_MESSAGE: &str =
    "Embedding model failed to initialize. Check server logs for details.";

struct LoadState {
    started_at: Instant,
    phase: LoadingPhase,
    cached: bool,
    error_message: Option<String>,
    progress_percent: Option<f32>,
    remaining_seconds: Option<u64>,
}

pub struct EmbeddingService {
    engine: Arc<RwLock<Option<Arc<EmbeddingEngine>>>>,
    cache: EmbeddingCache,
    config: EmbeddingConfig,
    status: Arc<AtomicU8>,
    load_state: Arc<RwLock<LoadState>>,
}

impl EmbeddingService {
    pub fn new(config: EmbeddingConfig) -> Self {
        Self {
            engine: Arc::new(RwLock::new(None)),
            cache: EmbeddingCache::new(config.cache_size),
            config,
            status: Arc::new(AtomicU8::new(STATUS_LOADING)),
            load_state: Arc::new(RwLock::new(LoadState {
                started_at: Instant::now(),
                phase: LoadingPhase::Starting,
                cached: false,
                error_message: None,
                progress_percent: None,
                remaining_seconds: None,
            })),
        }
    }

    pub fn start_loading(&self) {
        let engine_state = self.engine.clone();
        let status = self.status.clone();
        let load_state = self.load_state.clone();
        let model = self.config.model;
        let mrl_dim = self.config.mrl_dim;
        let cache_dir = self.config.cache_dir.clone();

        if model == ModelType::Mock {
            status.store(STATUS_READY, Ordering::SeqCst);
            tracing::info!("Mock embedding model ready");
            return;
        }

        let status_clone = status.clone();
        std::thread::Builder::new()
            .name("emb-loader".into())
            .stack_size(16 * 1024 * 1024) // 16 MB — Candle models need deep stack for tensor ops
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!("Failed to build embedding runtime: {}", e);
                        status_clone.store(STATUS_ERROR, Ordering::SeqCst);
                        return;
                    }
                };

                rt.block_on(async {
                    let mut state = load_state.write().await;
                    state.started_at = Instant::now();
                    state.phase = LoadingPhase::Starting;
                    drop(state);
                });

                if let Some(ref dir) = cache_dir {
                    rt.block_on(async {
                        let mut state = load_state.write().await;
                        state.phase = LoadingPhase::CleaningCache;
                        drop(state);
                    });

                    let cleanup_result = cleanup_model_cache(dir, model, &CleanupConfig::default());
                    if !cleanup_result.is_empty() {
                        tracing::info!(
                            "Cache cleanup: {} locks removed, {} incomplete files removed",
                            cleanup_result.locks_removed,
                            cleanup_result.incomplete_removed
                        );
                    }
                    for err in &cleanup_result.errors {
                        tracing::warn!("Cleanup error: {}", err);
                    }
                }

                tracing::info!("Loading embedding model: {:?}", model);

                match Self::load_model_with_tracking(model, mrl_dim, cache_dir, load_state.clone())
                {
                    Ok(engine) => {
                        rt.block_on(async {
                            let mut state = load_state.write().await;
                            state.phase = LoadingPhase::WarmingUp;
                            state.progress_percent = None;
                            drop(state);

                            if let Err(e) = engine.embed("warmup") {
                                tracing::warn!("Warmup failed (non-fatal): {}", e);
                            }

                            let mut guard = engine_state.write().await;
                            *guard = Some(Arc::new(engine));
                        });

                        status.store(STATUS_READY, Ordering::SeqCst);
                        let elapsed =
                            rt.block_on(async { load_state.read().await.started_at.elapsed() });
                        tracing::info!(
                            elapsed_sec = format!("{:.1}", elapsed.as_secs_f64()),
                            "Embedding model ready"
                        );
                    }
                    Err(e) => {
                        rt.block_on(async {
                            let mut state = load_state.write().await;
                            state.error_message = Some(REDACTED_LOAD_ERROR_MESSAGE.to_string());
                        });
                        tracing::error!("Failed to load embedding model: {}", e);
                        status.store(STATUS_ERROR, Ordering::SeqCst);
                    }
                }
            })
            .expect("Failed to spawn embedding loader thread");
    }

    /// Pure filesystem check — no network I/O.
    ///
    /// Replicates `CacheRepo::get()` from hf-hub (which is not accessible via `ApiRepo`).
    /// Path layout: `{cache_dir}/models--{org}--{repo}/refs/main` → commit hash →
    ///              `{cache_dir}/models--{org}--{repo}/snapshots/{hash}/{filename}`
    fn local_cache_path(cache_dir: &Path, repo_id: &str, filename: &str) -> Option<PathBuf> {
        let folder = format!("models--{}", repo_id.replace('/', "--"));
        let refs_path = cache_dir.join(&folder).join("refs").join("main");
        let commit_hash = std::fs::read_to_string(&refs_path).ok()?;
        let path = cache_dir
            .join(&folder)
            .join("snapshots")
            .join(commit_hash.trim())
            .join(filename);
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    fn load_model_with_tracking(
        model: ModelType,
        mrl_dim: Option<usize>,
        cache_dir: Option<PathBuf>,
        load_state: Arc<RwLock<LoadState>>,
    ) -> anyhow::Result<EmbeddingEngine> {
        use hf_hub::api::sync::ApiBuilder;
        use hf_hub_simple_progress::{sync::callback_builder, ProgressEvent};

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        let api = if let Some(ref dir) = cache_dir {
            std::fs::create_dir_all(dir)?;
            ApiBuilder::new().with_cache_dir(dir.clone()).build()?
        } else {
            hf_hub::api::sync::Api::new()?
        };

        let repo = api.model(model.repo_id().to_string());
        let repo_id = model.repo_id();

        // Check local cache (pure filesystem, no network) to set `cached` flag.
        // `repo.get()` is already cache-first, but we need the flag before calling it.
        let is_cached = cache_dir
            .as_ref()
            .and_then(|d| Self::local_cache_path(d, repo_id, "config.json"))
            .is_some();

        rt.block_on(async {
            let mut state = load_state.write().await;
            state.cached = is_cached;
            state.phase = LoadingPhase::FetchingConfig;
        });

        // `repo.get()` checks the local snapshot cache first; only downloads if missing.
        let config_path = repo.get("config.json")?;

        rt.block_on(async {
            let mut state = load_state.write().await;
            state.phase = LoadingPhase::FetchingTokenizer;
        });

        let tokenizer_path = repo.get("tokenizer.json")?;

        // For the weights file we want progress reporting on first download but want to
        // skip the network entirely (including the metadata HEAD request) when already cached.
        // `repo.download_with_progress()` always hits the network; `repo.get()` is
        // cache-first but doesn't report progress.  Solution: check the local cache path
        // manually, use `download_with_progress` only on a cache miss.
        let cached_weights = cache_dir
            .as_ref()
            .and_then(|d| Self::local_cache_path(d, repo_id, "model.safetensors"));

        rt.block_on(async {
            let mut state = load_state.write().await;
            state.phase = LoadingPhase::FetchingWeights;
            if cached_weights.is_none() {
                state.progress_percent = Some(0.0);
            }
        });

        let weights_path = if let Some(p) = cached_weights {
            tracing::info!("Model weights found in local cache, skipping download");
            p
        } else {
            tracing::info!("Downloading model weights from HuggingFace Hub...");
            let load_state_for_callback = load_state.clone();
            let callback = callback_builder(move |progress: ProgressEvent| {
                // Use try_write to avoid needing a runtime — skip update if lock is contended
                if let Ok(mut state) = load_state_for_callback.try_write() {
                    state.progress_percent = Some(progress.percentage * 100.0);
                    let remaining = progress.remaining_time.as_secs();
                    state.remaining_seconds = if remaining > 0 { Some(remaining) } else { None };
                }
            });
            repo.download_with_progress("model.safetensors", callback)?
        };

        rt.block_on(async {
            let mut state = load_state.write().await;
            state.phase = LoadingPhase::LoadingModel;
            state.progress_percent = None;
        });

        EmbeddingEngine::from_files(
            &EmbeddingConfig {
                model,
                mrl_dim,
                cache_size: 0,
                batch_size: 1,
                cache_dir: cache_dir.clone(),
            },
            &config_path,
            &tokenizer_path,
            &weights_path,
        )
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let model_ver = self.config.model.repo_id();
        if let Some(cached) = self.cache.get(text, model_ver) {
            return Ok(cached);
        }

        if self.config.model == ModelType::Mock {
            let dim = self.config.output_dim();
            let mut vec = vec![0.0; dim];
            let hash = blake3::hash(text.as_bytes());
            let bytes = hash.as_bytes();
            for i in 0..dim.min(32) {
                vec[i] = (bytes[i % 32] as f32) / 255.0;
            }
            self.cache.put(text, model_ver, vec.clone());
            return Ok(vec);
        }

        // Brief lock — clone the inner Arc, then drop the guard immediately.
        // This ensures writers (model hot-reload) are never blocked during inference.
        let engine = {
            let guard = self.engine.read().await;
            Arc::clone(guard.as_ref().ok_or(AppError::EmbeddingNotReady)?)
        }; // guard dropped — writers unblocked

        // Offload CPU-bound neural-net inference to the blocking thread pool so
        // the tokio worker thread is freed for other async tasks.
        let text_owned = text.to_string();
        let embedding = tokio::task::spawn_blocking(move || engine.embed(&text_owned))
            .await
            .map_err(|e| AppError::Internal(format!("embed task panicked: {e}").into()))?
            .map_err(|e| AppError::Embedding(e.to_string()))?;

        self.cache.put(text, model_ver, embedding.clone());

        Ok(embedding)
    }

    pub async fn status(&self) -> EmbeddingStatus {
        match self.status.load(Ordering::SeqCst) {
            STATUS_LOADING => {
                let state = self.load_state.read().await;
                let elapsed = state.started_at.elapsed().as_secs();

                EmbeddingStatus::Loading {
                    phase: state.phase.clone(),
                    elapsed_seconds: elapsed,
                    eta_seconds: state.remaining_seconds,
                    cached: state.cached,
                    progress_percent: state.progress_percent,
                    downloaded_mb: None,
                    total_mb: None,
                }
            }
            STATUS_READY => EmbeddingStatus::Ready,
            _ => {
                let state = self.load_state.read().await;
                EmbeddingStatus::Error {
                    message: state
                        .error_message
                        .clone()
                        .unwrap_or_else(|| REDACTED_LOAD_ERROR_MESSAGE.to_string()),
                }
            }
        }
    }

    pub fn is_ready(&self) -> bool {
        self.status.load(Ordering::SeqCst) == STATUS_READY
    }

    pub async fn wait_for_ready(&self) -> Result<()> {
        if self.is_ready() {
            return Ok(());
        }

        tracing::info!("Waiting for embedding model to load...");
        loop {
            match self.status.load(Ordering::SeqCst) {
                STATUS_READY => return Ok(()),
                STATUS_ERROR => {
                    let state = self.load_state.read().await;
                    let msg = state.error_message.clone().unwrap_or_default();
                    return Err(AppError::Embedding(format!("Model load failed: {}", msg)));
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
            }
        }
    }

    /// Wait for the embedding model to become ready, up to `timeout`.
    ///
    /// Returns `Ok(true)` if the model became ready within the timeout.
    /// Returns `Ok(false)` if the timeout elapsed and the model is still loading.
    /// Returns `Err` if the model failed to load.
    ///
    /// This is the correct primitive for tool handlers: instead of immediately
    /// rejecting calls with "model not ready", they can absorb the startup
    /// latency transparently, keeping the MCP session alive on fresh machines
    /// where the model must be downloaded.
    pub async fn wait_for_ready_timeout(&self, timeout: std::time::Duration) -> Result<bool> {
        if self.is_ready() {
            return Ok(true);
        }

        let deadline = tokio::time::Instant::now() + timeout;
        tracing::info!(
            timeout_secs = timeout.as_secs(),
            "Tool call waiting for embedding model to load (lazy init)..."
        );

        loop {
            match self.status.load(Ordering::SeqCst) {
                STATUS_READY => {
                    tracing::info!("Embedding model became ready, proceeding with tool call");
                    return Ok(true);
                }
                STATUS_ERROR => {
                    let state = self.load_state.read().await;
                    let msg = state.error_message.clone().unwrap_or_default();
                    return Err(AppError::Embedding(format!("Model load failed: {}", msg)));
                }
                _ => {
                    if tokio::time::Instant::now() >= deadline {
                        tracing::warn!(
                            timeout_secs = timeout.as_secs(),
                            "Tool call timed out waiting for embedding model"
                        );
                        return Ok(false);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    }

    pub fn model(&self) -> ModelType {
        self.config.model
    }

    pub fn dimensions(&self) -> usize {
        self.config.output_dim()
    }

    pub fn get_engine(&self) -> Arc<RwLock<Option<Arc<EmbeddingEngine>>>> {
        self.engine.clone()
    }
}
