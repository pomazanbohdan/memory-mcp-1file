use std::path::Path;

use anyhow::{anyhow, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use candle_transformers::models::gemma2::{Config as Gemma2Config, Model as Gemma2Model};
use candle_transformers::models::qwen3::{Config as Qwen3Config, Model as Qwen3Model};
use hf_hub::api::sync::Api;
use tokenizers::Tokenizer;

/// Maximum token sequence length for BERT models.
/// Attention is O(n²) — exceeding this causes massive memory usage.
const MAX_SEQ_LEN_BERT: usize = 512;
const MAX_SEQ_LEN_QWEN3: usize = 256; // MRL capable Qwen3; most code chunks < 256 tokens

use super::config::{EmbeddingConfig, EngineBackend};

enum InnerModel {
    Bert(BertModel),
    Qwen3(std::sync::Mutex<Qwen3Model>),
    Gemma(std::sync::Mutex<Gemma2Model>),
    Mock,
}

fn l2_normalize(t: &Tensor) -> Result<Tensor> {
    let norm = t.sqr()?.sum_keepdim(1)?.sqrt()?.clamp(1e-9_f64, f64::MAX)?;
    t.broadcast_div(&norm).map_err(Into::into)
}

pub struct EmbeddingEngine {
    inner: InnerModel,
    tokenizer: Option<Tokenizer>,
    device: Device,
    dimensions: usize,
    mrl_dim: Option<usize>,
}

impl EmbeddingEngine {
    pub fn new(config: &EmbeddingConfig) -> Result<Self> {
        let device = Device::Cpu;
        let base_dims = config.model.base_dimensions();
        let backend = config.model.engine_backend();

        if backend == EngineBackend::Mock {
            return Ok(Self {
                inner: InnerModel::Mock,
                tokenizer: None,
                device,
                dimensions: base_dims,
                mrl_dim: config.mrl_dim,
            });
        }

        let api = Api::new()?;
        let repo = api.model(config.model.repo_id().to_string());

        let config_filename = repo.get("config.json")?;
        let tokenizer_filename = repo.get("tokenizer.json")?;
        let weights_filename = repo.get("model.safetensors")?;

        Self::from_files(
            config,
            &config_filename,
            &tokenizer_filename,
            &weights_filename,
        )
    }

    pub fn from_files(
        config: &EmbeddingConfig,
        config_path: &Path,
        tokenizer_path: &Path,
        weights_path: &Path,
    ) -> Result<Self> {
        let device = Device::Cpu;
        let mut tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow!("Failed to load tokenizer: {}", e))?;

        // Enable padding if not already present
        if tokenizer.get_padding().is_none() {
            let pad_id = tokenizer.token_to_id("[PAD]").unwrap_or(0);
            let pad_params = tokenizers::PaddingParams {
                strategy: tokenizers::PaddingStrategy::BatchLongest,
                direction: tokenizers::PaddingDirection::Right,
                pad_to_multiple_of: None,
                pad_id,
                pad_type_id: 0,
                pad_token: String::from("[PAD]"),
            };
            tokenizer.with_padding(Some(pad_params));
        }

        let vb =
            unsafe { VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, &device)? };

        let backend = config.model.engine_backend();
        let (inner, actual_dim) = match backend {
            EngineBackend::Bert => {
                let bert_cfg: BertConfig = serde_json::from_slice(&std::fs::read(config_path)?)?;
                let dim = bert_cfg.hidden_size;
                (InnerModel::Bert(BertModel::load(vb, &bert_cfg)?), dim)
            }
            EngineBackend::Qwen3 => {
                let qwen_cfg: Qwen3Config = serde_json::from_slice(&std::fs::read(config_path)?)?;
                let dim = qwen_cfg.hidden_size;
                // Qwen3-Embedding-0.6B safetensors stores tensors WITHOUT "model." prefix
                // (e.g. "embed_tokens.weight" instead of "model.embed_tokens.weight"),
                // but candle's Qwen3Model::new() internally uses vb.pp("model.embed_tokens").
                // Fix: strip the "model." prefix that candle adds during lookup.
                let vb_fixed = vb
                    .rename_f(|name: &str| name.strip_prefix("model.").unwrap_or(name).to_string());
                (
                    InnerModel::Qwen3(std::sync::Mutex::new(Qwen3Model::new(&qwen_cfg, vb_fixed)?)),
                    dim,
                )
            }
            EngineBackend::Gemma => {
                let gemma_cfg: Gemma2Config = serde_json::from_slice(&std::fs::read(config_path)?)?;
                let dim = gemma_cfg.hidden_size;
                let vb_fixed = vb
                    .rename_f(|name: &str| name.strip_prefix("model.").unwrap_or(name).to_string());
                (
                    InnerModel::Gemma(std::sync::Mutex::new(Gemma2Model::new(
                        false, &gemma_cfg, vb_fixed,
                    )?)),
                    dim,
                )
            }
            EngineBackend::Mock => (InnerModel::Mock, config.model.base_dimensions()),
        };

        let expected_dim = config.model.base_dimensions();
        if actual_dim != expected_dim {
            tracing::error!(
                model = %config.model,
                actual_dim,
                expected_dim,
                "Model hidden_size does not match base_dimensions(). \
                 Update ModelType::base_dimensions() to return {}.",
                actual_dim
            );
            anyhow::bail!(
                "Dimension mismatch: model {} has hidden_size={} but base_dimensions()={}",
                config.model,
                actual_dim,
                expected_dim
            );
        }

        Ok(Self {
            inner,
            tokenizer: Some(tokenizer),
            device,
            dimensions: actual_dim,
            mrl_dim: config.mrl_dim,
        })
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        match &self.inner {
            InnerModel::Mock => {
                let hash = blake3::hash(text.as_bytes());
                let bytes = hash.as_bytes();
                let mut vec = vec![0.0f32; self.dimensions];
                for (i, &b) in bytes.iter().enumerate() {
                    vec[i % self.dimensions] += (b as f32) / 255.0;
                }
                self.apply_mrl(vec)
            }
            _ => {
                let tokenizer = self.tokenizer.as_ref().unwrap();
                let tokens = tokenizer
                    .encode(text, true)
                    .map_err(|e| anyhow!("Tokenization failed: {}", e))?;

                let mut token_ids = tokens.get_ids().to_vec();
                let max_len = match self.inner {
                    InnerModel::Qwen3(_) => MAX_SEQ_LEN_QWEN3,
                    InnerModel::Gemma(_) => 512,
                    _ => MAX_SEQ_LEN_BERT,
                };

                if token_ids.len() > max_len {
                    token_ids.truncate(max_len);
                }
                if token_ids.is_empty() {
                    anyhow::bail!("Cannot embed empty token sequence");
                }

                match &self.inner {
                    InnerModel::Bert(model) => {
                        let token_ids = Tensor::new(vec![token_ids.clone()], &self.device)?;
                        let token_type_ids =
                            Tensor::zeros(token_ids.shape(), DType::U32, &self.device)?;
                        let hidden = model.forward(&token_ids, &token_type_ids, None)?;

                        let (_n_batch, n_tokens, _hidden_size) = hidden.dims3()?;
                        let sum = hidden.sum(1)?;
                        let mean_pooled = (sum / (n_tokens as f64))?;

                        let normalized = l2_normalize(&mean_pooled)?;

                        let vec = normalized.squeeze(0)?.to_vec1::<f32>()?;
                        self.apply_mrl(vec)
                    }
                    InnerModel::Qwen3(model_mutex) => {
                        let input_ids = Tensor::new(vec![token_ids.clone()], &self.device)?;
                        let mut model_mut = model_mutex
                            .lock()
                            .map_err(|_| anyhow::anyhow!("Mutex poisoned"))?;
                        // Clear KV cache before each independent embedding request.
                        // Without this, the KV cache accumulates across calls, causing
                        // `broadcast_add` to fail: the attention mask is (b,1,L,L) but
                        // the KV scores are (b,H,L,N+L) after N cached tokens.
                        model_mut.clear_kv_cache();
                        let hidden = model_mut.forward(&input_ids, 0)?;

                        let seq_len = hidden.dim(1)?;
                        let embedding = hidden.narrow(1, seq_len - 1, 1)?.squeeze(1)?;

                        let normalized = l2_normalize(&embedding)?;

                        let vec = normalized.squeeze(0)?.to_vec1::<f32>()?;
                        self.apply_mrl(vec)
                    }
                    InnerModel::Gemma(model_mutex) => {
                        let input_ids = Tensor::new(vec![token_ids.clone()], &self.device)?;
                        let mut model_mut = model_mutex
                            .lock()
                            .map_err(|_| anyhow::anyhow!("Mutex poisoned"))?;
                        model_mut.clear_kv_cache();
                        // Use forward_embeds to get hidden states [b, seq_len, hidden_size]
                        // instead of forward which returns lm_head logits [b, 1, vocab_size].
                        let hidden = model_mut.forward_embeds(&input_ids, 0)?;

                        // Mean pooling: average all token hidden states.
                        // EmbeddingGemma is designed for mean pooling (not last-token),
                        // per Google's ablation studies. Last-token without EOS produces
                        // degenerate embeddings for short code chunks.
                        let (_n_batch, n_tokens, _hidden_size) = hidden.dims3()?;
                        let embedding = (hidden.sum(1)? / (n_tokens as f64))?;

                        let normalized = l2_normalize(&embedding)?;

                        let vec = normalized.squeeze(0)?.to_vec1::<f32>()?;
                        self.apply_mrl(vec)
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Option<Vec<f32>>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Dynamic chunk sizing based on actual sequence length in this batch.
        // Goal: keep peak RAM from forward pass within 800MB budget.
        //
        // RAM model per item: max_seq × hidden × layers × bytes × safety_factor
        //   Gemma2: 768 hidden, 26 layers, factor=5 (hidden + KV + FFN + grad buffers)
        //   For max_seq=512: ~49MB/item → safe_chunk = 800MB/49MB ≈ 16 → clamp 8
        //   For max_seq=256: ~24MB/item → safe_chunk ≈ 33 → clamp 8
        //   For max_seq=128: ~12MB/item → safe_chunk ≈ 64 → clamp 8
        //
        // Baseline container: ~1.7GB. Budget 800MB → peak ≤ 2.5GB (safe within 4GB).

        let safe_chunk = if let Some(tokenizer) = &self.tokenizer {
            // Estimate max_seq from first item (cheap, single encode)
            let sample = texts.first().map(|s| s.as_str()).unwrap_or("");
            let est_seq = tokenizer
                .encode(sample, true)
                .map(|enc| enc.get_ids().len().min(512))
                .unwrap_or(256);

            let hidden = self.dimensions();
            let layers: usize = match &self.inner {
                InnerModel::Gemma(_) => 26,
                InnerModel::Qwen3(_) => 28,
                _ => 12,
            };
            let bytes_per_item = est_seq * hidden * layers * 5; // factor=5 safety
            let ram_budget: usize = 800 * 1024 * 1024; // 800MB
            (ram_budget / bytes_per_item.max(1)).clamp(1, 8)
        } else {
            // Mock model — no real tensors, no OOM risk
            texts.len()
        };

        if texts.len() > safe_chunk {
            tracing::debug!(
                total = texts.len(),
                safe_chunk,
                "embed_batch: dynamic chunking to prevent OOM"
            );
            let mut all_results = Vec::with_capacity(texts.len());
            for chunk in texts.chunks(safe_chunk) {
                let chunk_results = self.embed_batch_inner(chunk)?;
                all_results.extend(chunk_results);
            }
            return Ok(all_results);
        }

        self.embed_batch_inner(texts)
    }

    fn embed_batch_inner(&self, texts: &[String]) -> Result<Vec<Option<Vec<f32>>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        match &self.inner {
            InnerModel::Mock => {
                let mut results = Vec::with_capacity(texts.len());
                for text in texts {
                    results.push(Some(self.embed(text)?));
                }
                Ok(results)
            }
            _ => {
                let tokenizer = self.tokenizer.as_ref().unwrap();
                let encodes = tokenizer
                    .encode_batch(texts.to_vec(), true)
                    .map_err(|e| anyhow!("Batch tokenization failed: {}", e))?;

                let max_len = match self.inner {
                    InnerModel::Qwen3(_) => MAX_SEQ_LEN_QWEN3,
                    InnerModel::Gemma(_) => 512,
                    _ => MAX_SEQ_LEN_BERT,
                };

                let unpadded_token_ids: Vec<Vec<u32>> = encodes
                    .into_iter()
                    .map(|enc| {
                        let mut ids = enc.get_ids().to_vec();
                        if ids.len() > max_len {
                            ids.truncate(max_len);
                        }
                        ids
                    })
                    .collect();

                let actual_lengths: Vec<usize> =
                    unpadded_token_ids.iter().map(|ids| ids.len()).collect();
                let max_seq_len_in_batch = actual_lengths.iter().copied().max().unwrap_or(0);

                let mut token_ids = unpadded_token_ids.clone();
                for ids in &mut token_ids {
                    ids.resize(max_seq_len_in_batch, 0); // 0 is usually PAD
                }

                match &self.inner {
                    InnerModel::Bert(model) => {
                        let attention_mask: Vec<Vec<u32>> = token_ids
                            .iter()
                            .map(|ids| ids.iter().map(|&id| if id == 0 { 0 } else { 1 }).collect())
                            .collect();

                        let token_ids_tensor = Tensor::new(token_ids, &self.device)?;
                        let attention_mask_tensor = Tensor::new(attention_mask, &self.device)?;
                        let token_type_ids =
                            Tensor::zeros(token_ids_tensor.shape(), DType::U32, &self.device)?;

                        let hidden = model.forward(&token_ids_tensor, &token_type_ids, None)?;
                        let (_batch_size, _seq_len, _hidden_size) = hidden.dims3()?;

                        let mask_expanded = attention_mask_tensor
                            .unsqueeze(2)?
                            .broadcast_as(hidden.shape())?
                            .to_dtype(DType::F32)?;
                        let hidden_masked = (hidden * &mask_expanded)?;
                        let sum_hidden = hidden_masked.sum(1)?;
                        let sum_mask = mask_expanded.sum(1)?.clamp(1e-9, f64::MAX)?;
                        let mean_pooled = (sum_hidden / sum_mask)?;

                        let normalized = l2_normalize(&mean_pooled)?;

                        let mut results = Vec::with_capacity(texts.len());
                        for i in 0..texts.len() {
                            let vec = normalized.get(i)?.to_vec1::<f32>()?;
                            results.push(Some(self.apply_mrl(vec)?));
                        }
                        Ok(results)
                    }
                    InnerModel::Qwen3(model_mutex) => {
                        let mut model_mut = model_mutex
                            .lock()
                            .map_err(|_| anyhow::anyhow!("Mutex poisoned"))?;

                        // Qwen3 is decoder-only — causal mask in candle does NOT
                        // support batch dim > 1. Process items sequentially but
                        // keep KV cache cleared between items.
                        let mut results = Vec::with_capacity(texts.len());
                        for (ids, &actual_len) in
                            unpadded_token_ids.iter().zip(actual_lengths.iter())
                        {
                            if actual_len == 0 {
                                tracing::warn!("Skipping empty token sequence in Qwen3 batch");
                                results.push(None);
                                continue;
                            }
                            model_mut.clear_kv_cache();
                            let input = Tensor::new(ids.as_slice(), &self.device)?.unsqueeze(0)?;
                            let hidden = model_mut.forward(&input, 0)?;

                            let embedding = hidden.narrow(1, actual_len - 1, 1)?.squeeze(1)?;
                            let normalized = l2_normalize(&embedding)?;
                            let vec = normalized.squeeze(0)?.to_vec1::<f32>()?;
                            results.push(Some(self.apply_mrl(vec)?));
                        }
                        Ok(results)
                    }
                    InnerModel::Gemma(model_mutex) => {
                        let mut model_mut = model_mutex
                            .lock()
                            .map_err(|_| anyhow::anyhow!("Mutex poisoned"))?;

                        // Handle edge case: any empty sequences → sequential fallback
                        if actual_lengths.contains(&0) {
                            let mut results = Vec::with_capacity(texts.len());
                            for (ids, &actual_len) in
                                unpadded_token_ids.iter().zip(actual_lengths.iter())
                            {
                                if actual_len == 0 {
                                    tracing::warn!("Skipping empty token sequence in Gemma batch");
                                    results.push(None);
                                    continue;
                                }
                                model_mut.clear_kv_cache();
                                let input =
                                    Tensor::new(ids.as_slice(), &self.device)?.unsqueeze(0)?;
                                let hidden = model_mut.forward_embeds(&input, 0)?;
                                let hidden_unpadded = hidden.narrow(1, 0, actual_len)?;
                                let embedding = (hidden_unpadded.sum(1)? / (actual_len as f64))?;
                                let normalized = l2_normalize(&embedding)?;
                                let vec = normalized.squeeze(0)?.to_vec1::<f32>()?;
                                results.push(Some(self.apply_mrl(vec)?));
                            }
                            return Ok(results);
                        }

                        // True batch forward: single forward pass for all items.
                        // Gemma2 forward_embeds supports [B, seq_len] input — causal mask
                        // is created per-batch internally. Mean pooling makes this safe:
                        // each real token gets identical hidden state regardless of batch size,
                        // and padding tokens are excluded via attention mask.
                        model_mut.clear_kv_cache();

                        // Build attention mask: 1 for real tokens, 0 for padding
                        let attention_mask: Vec<Vec<u32>> = actual_lengths
                            .iter()
                            .map(|&len| {
                                (0..max_seq_len_in_batch)
                                    .map(|i| if i < len { 1 } else { 0 })
                                    .collect()
                            })
                            .collect();

                        let token_ids_tensor = Tensor::new(token_ids, &self.device)?;
                        let attention_mask_tensor = Tensor::new(attention_mask, &self.device)?;

                        // Single forward pass: [B, seq] → [B, seq, hidden]
                        let hidden = model_mut.forward_embeds(&token_ids_tensor, 0)?;

                        // Masked mean pooling (same strategy as BERT path)
                        let mask_expanded = attention_mask_tensor
                            .unsqueeze(2)?
                            .broadcast_as(hidden.shape())?
                            .to_dtype(DType::F32)?;
                        let hidden_masked = (hidden * &mask_expanded)?;
                        let sum_hidden = hidden_masked.sum(1)?;
                        let sum_mask = mask_expanded.sum(1)?.clamp(1e-9, f64::MAX)?;
                        let mean_pooled = (sum_hidden / sum_mask)?;

                        let normalized = l2_normalize(&mean_pooled)?;

                        let mut results = Vec::with_capacity(texts.len());
                        for i in 0..texts.len() {
                            let vec = normalized.get(i)?.to_vec1::<f32>()?;
                            results.push(Some(self.apply_mrl(vec)?));
                        }
                        Ok(results)
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    pub fn dimensions(&self) -> usize {
        self.mrl_dim.unwrap_or(self.dimensions)
    }

    fn apply_mrl(&self, mut vec: Vec<f32>) -> Result<Vec<f32>> {
        if let Some(dim) = self.mrl_dim {
            if dim < vec.len() {
                vec.truncate(dim);
                let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm > 1e-9_f32 {
                    for v in &mut vec {
                        *v /= norm;
                    }
                }
            }
        }
        Ok(vec)
    }
}
