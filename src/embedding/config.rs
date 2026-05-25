/// Which inference backend the engine should use for a given model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EngineBackend {
    /// BERT-family encoder (e5, nomic-v1.5, bge-m3).
    Bert,
    /// Decoder-only Qwen3 backbone with last-token pooling.
    Qwen3,
    /// Gemma3 text encoder.
    #[default]
    Gemma,
    /// Hash-based deterministic stub for tests.
    Mock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelType {
    /// intfloat/multilingual-e5-small — 384d, ~85 MB Q4. Legacy lightweight option.
    E5Small,
    /// intfloat/multilingual-e5-base — 768d, ~180 MB. Legacy; kept for backward compat. Default.
    #[default]
    E5Multi,
    /// nomic-ai/nomic-embed-text-v1.5 — 768d, ~270 MB. Long-context BERT-compatible.
    Nomic,
    /// BAAI/bge-m3 — 1024d, ~420 MB. Hybrid dense+sparse+colbert retrieval.
    BgeM3,
    /// Qwen/Qwen3-Embedding-0.6B — 1024d, ~1.2 GB. Top open-source 2026, MRL, 32K ctx.
    Qwen3,
    /// unsloth/embeddinggemma-300m-qat-q4_0-unquantized — 768d, ~195 MB.
    Gemma,
    Mock,
}

impl ModelType {
    pub fn repo_id(&self) -> &'static str {
        match self {
            Self::E5Small => "intfloat/multilingual-e5-small",
            Self::E5Multi => "intfloat/multilingual-e5-base",
            Self::Nomic => "nomic-ai/nomic-embed-text-v1.5",
            Self::BgeM3 => "BAAI/bge-m3",
            Self::Qwen3 => "Qwen/Qwen3-Embedding-0.6B",
            Self::Gemma => "unsloth/embeddinggemma-300m-qat-q4_0-unquantized",
            Self::Mock => "mock",
        }
    }

    /// Native embedding dimensionality of the model (before MRL truncation).
    pub fn base_dimensions(&self) -> usize {
        match self {
            Self::E5Small => 384,
            Self::E5Multi => 768,
            Self::Nomic => 768,
            Self::BgeM3 => 1024,
            Self::Qwen3 => 1024,
            Self::Gemma => 768,
            Self::Mock => 768,
        }
    }

    /// Whether the model supports Matryoshka Representation Learning (MRL) —
    /// i.e. the first N dimensions are meaningful on their own after L2 normalisation.
    pub fn supports_mrl(&self) -> bool {
        matches!(self, Self::Qwen3 | Self::Gemma)
    }

    /// Inference backend required for this model.
    pub fn engine_backend(&self) -> EngineBackend {
        match self {
            Self::E5Small | Self::E5Multi | Self::Nomic | Self::BgeM3 => EngineBackend::Bert,
            Self::Qwen3 => EngineBackend::Qwen3,
            Self::Gemma => EngineBackend::Gemma,
            Self::Mock => EngineBackend::Mock,
        }
    }

    /// True when the model's license requires explicit user acceptance (non-Apache/MIT).
    pub fn requires_license_agreement(&self) -> bool {
        false
    }

    /// Human-readable approximate download size.
    pub fn approx_size(&self) -> &'static str {
        match self {
            Self::E5Small => "~85 MB",
            Self::E5Multi => "~180 MB",
            Self::Nomic => "~270 MB",
            Self::Gemma => "~195 MB",
            Self::BgeM3 => "~420 MB",
            Self::Qwen3 => "~1.2 GB",
            Self::Mock => "0 MB",
        }
    }
}

impl std::str::FromStr for ModelType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "e5_small" | "e5-small"                          => Ok(Self::E5Small),
            "e5_multi" | "e5-multi" | "e5_base" | "e5-base" => Ok(Self::E5Multi),
            "nomic"                                          => Ok(Self::Nomic),
            "bge_m3"   | "bge-m3"                            => Ok(Self::BgeM3),
            "qwen3"    | "qwen-3"                            => Ok(Self::Qwen3),
            "gemma"                                          => Ok(Self::Gemma),
            "mock"                                           => Ok(Self::Mock),
            _ => Err(format!(
                "Unknown model: '{}'. Valid values: e5_small, e5_multi, nomic, bge_m3, qwen3, gemma",
                s
            )),
        }
    }
}

impl std::fmt::Display for ModelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::E5Small => write!(f, "e5_small"),
            Self::E5Multi => write!(f, "e5_multi"),
            Self::Nomic => write!(f, "nomic"),
            Self::BgeM3 => write!(f, "bge_m3"),
            Self::Qwen3 => write!(f, "qwen3"),
            Self::Gemma => write!(f, "gemma"),
            Self::Mock => write!(f, "mock"),
        }
    }
}

// ---------------------------------------------------------------------------
// Config errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ConfigError {
    NotSupported(ModelType),
    DimZero,
    DimExceedsBase { requested: usize, base: usize },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSupported(m) => {
                write!(f, "Model '{}' does not support MRL dimension truncation", m)
            }
            Self::DimZero => write!(f, "mrl_dim must be > 0"),
            Self::DimExceedsBase { requested, base } => write!(
                f,
                "mrl_dim {} exceeds model base dimensions {}",
                requested, base
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

// ---------------------------------------------------------------------------
// EmbeddingConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    pub model: ModelType,
    /// MRL output dimension. `None` = use model's base dimensions.
    /// Only valid for models where `supports_mrl()` is true.
    pub mrl_dim: Option<usize>,
    pub cache_size: usize,
    pub batch_size: usize,
    pub cache_dir: Option<std::path::PathBuf>,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: ModelType::default(), // single source of truth
            mrl_dim: None,
            cache_size: 1000,
            batch_size: 32,
            cache_dir: None,
        }
    }
}

impl EmbeddingConfig {
    /// Actual output dimensionality after optional MRL truncation.
    pub fn output_dim(&self) -> usize {
        self.mrl_dim.unwrap_or_else(|| self.model.base_dimensions())
    }

    /// Validate MRL settings. Call once after construction.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(dim) = self.mrl_dim {
            if dim == 0 {
                return Err(ConfigError::DimZero);
            }
            if !self.model.supports_mrl() {
                return Err(ConfigError::NotSupported(self.model));
            }
            if dim > self.model.base_dimensions() {
                return Err(ConfigError::DimExceedsBase {
                    requested: dim,
                    base: self.model.base_dimensions(),
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_model_type_from_str() {
        assert_eq!(ModelType::from_str("e5-small").unwrap(), ModelType::E5Small);
        assert_eq!(ModelType::from_str("E5_MULTI").unwrap(), ModelType::E5Multi);
        assert_eq!(ModelType::from_str("nomic").unwrap(), ModelType::Nomic);
        assert_eq!(ModelType::from_str("bge-m3").unwrap(), ModelType::BgeM3);
        assert_eq!(ModelType::from_str("qwen3").unwrap(), ModelType::Qwen3);
        assert_eq!(ModelType::from_str("qwen-3").unwrap(), ModelType::Qwen3);
        assert_eq!(ModelType::from_str("gemma").unwrap(), ModelType::Gemma);
        assert!(ModelType::from_str("unknown").is_err());
    }

    #[test]
    fn test_display_roundtrip() {
        for m in [
            ModelType::E5Small,
            ModelType::E5Multi,
            ModelType::Nomic,
            ModelType::BgeM3,
            ModelType::Qwen3,
            ModelType::Gemma,
        ] {
            let s = m.to_string();
            assert_eq!(ModelType::from_str(&s).unwrap(), m);
        }
    }

    #[test]
    fn test_dimensions() {
        assert_eq!(ModelType::E5Small.base_dimensions(), 384);
        assert_eq!(ModelType::E5Multi.base_dimensions(), 768);
        assert_eq!(ModelType::BgeM3.base_dimensions(), 1024);
        assert_eq!(ModelType::Qwen3.base_dimensions(), 1024);
        assert_eq!(ModelType::Gemma.base_dimensions(), 768);
    }

    #[test]
    fn test_mrl_support() {
        assert!(ModelType::Qwen3.supports_mrl());
        assert!(ModelType::Gemma.supports_mrl());
        assert!(!ModelType::BgeM3.supports_mrl());
        assert!(!ModelType::E5Multi.supports_mrl());
    }

    #[test]
    fn test_default_is_e5_multi() {
        assert_eq!(ModelType::default(), ModelType::E5Multi);
        assert_eq!(EmbeddingConfig::default().model, ModelType::E5Multi);
    }

    #[test]
    fn test_output_dim() {
        let cfg = EmbeddingConfig {
            model: ModelType::Gemma,
            mrl_dim: Some(512),
            ..Default::default()
        };
        assert_eq!(cfg.output_dim(), 512);

        let cfg2 = EmbeddingConfig::default();
        assert_eq!(cfg2.output_dim(), 768);
    }

    #[test]
    fn test_validate_mrl() {
        // valid
        let cfg = EmbeddingConfig {
            model: ModelType::Qwen3,
            mrl_dim: Some(512),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());

        // dim=0
        let cfg = EmbeddingConfig {
            model: ModelType::Qwen3,
            mrl_dim: Some(0),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());

        // exceeds base
        let cfg = EmbeddingConfig {
            model: ModelType::Qwen3,
            mrl_dim: Some(2048),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());

        // unsupported model
        let cfg = EmbeddingConfig {
            model: ModelType::BgeM3,
            mrl_dim: Some(512),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_gemma_license_flag() {
        assert!(!ModelType::Gemma.requires_license_agreement());
        assert!(!ModelType::Qwen3.requires_license_agreement());
    }

    #[test]
    fn test_default_config() {
        let config = EmbeddingConfig::default();
        assert_eq!(config.model, ModelType::Gemma);
        assert_eq!(config.cache_size, 1000);
        assert!(config.mrl_dim.is_none());
    }
}
