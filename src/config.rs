use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Corebox,
    OpenaiCompatible,
    Gemini,
    Ollama,
    Mock,
}

impl Default for ProviderKind {
    fn default() -> Self {
        Self::Ollama
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub autocomplete_model: Option<String>,
    pub edit_model: Option<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    15000
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::Ollama,
            base_url: None,
            api_key: None,
            autocomplete_model: None,
            edit_model: None,
            timeout_ms: default_timeout_ms(),
        }
    }
}

impl ProviderConfig {
    pub fn effective_base_url(&self) -> String {
        if let Some(url) = &self.base_url {
            if !url.trim().is_empty() {
                return url.trim_end_matches('/').to_string();
            }
        }
        match self.kind {
            ProviderKind::Corebox => "https://corebox.fyi/v1".to_string(),
            ProviderKind::OpenaiCompatible => "https://api.openai.com/v1".to_string(),
            ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta".to_string(),
            ProviderKind::Ollama => "http://localhost:11434".to_string(),
            ProviderKind::Mock => "mock://local".to_string(),
        }
    }

    pub fn effective_autocomplete_model(&self) -> String {
        if let Some(m) = &self.autocomplete_model {
            if !m.trim().is_empty() {
                return m.clone();
            }
        }
        match self.kind {
            ProviderKind::Corebox => "gemini-3.5-flash-lite".to_string(),
            ProviderKind::OpenaiCompatible => "gpt-4o-mini".to_string(),
            ProviderKind::Gemini => "gemini-2.5-flash".to_string(),
            ProviderKind::Ollama => "qwen2.5:1.5b".to_string(),
            ProviderKind::Mock => "mock-fast-v1".to_string(),
        }
    }

    pub fn effective_edit_model(&self) -> String {
        if let Some(m) = &self.edit_model {
            if !m.trim().is_empty() {
                return m.clone();
            }
        }
        match self.kind {
            ProviderKind::Corebox => "gemini-3.5-flash-lite".to_string(),
            ProviderKind::OpenaiCompatible => "gpt-4o-mini".to_string(),
            ProviderKind::Gemini => "gemini-2.5-flash".to_string(),
            ProviderKind::Ollama => "qwen2.5:1.5b".to_string(),
            ProviderKind::Mock => "mock-edit-v1".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorConfig {
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default = "default_max_completion_tokens")]
    pub max_completion_tokens: u32,
    #[serde(default = "default_max_context_chars")]
    pub max_context_chars: usize,
    #[serde(default = "default_cache_size")]
    pub cache_size: usize,
    #[serde(default = "default_min_prefix_chars")]
    pub min_prefix_chars: usize,
}

fn default_debounce_ms() -> u64 {
    350
}
fn default_max_completion_tokens() -> u32 {
    48
}
fn default_max_context_chars() -> usize {
    600
}
fn default_cache_size() -> usize {
    128
}
fn default_min_prefix_chars() -> usize {
    4
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            debounce_ms: default_debounce_ms(),
            max_completion_tokens: default_max_completion_tokens(),
            max_context_chars: default_max_context_chars(),
            cache_size: default_cache_size(),
            min_prefix_chars: default_min_prefix_chars(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyConfig {
    #[serde(default = "default_blocked_apps")]
    pub blocked_apps: Vec<String>,
    #[serde(default = "default_true")]
    pub block_secure_fields: bool,
    #[serde(default = "default_true")]
    pub block_sensitive_patterns: bool,
}

fn default_true() -> bool {
    true
}

fn default_blocked_apps() -> Vec<String> {
    vec![
        "1password",
        "bitwarden",
        "keepassxc",
        "keychain access",
        "связка ключей",
        "terminal",
        "iterm2",
        "warp",
        "alacritty",
        "kitty",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            blocked_apps: default_blocked_apps(),
            block_secure_fields: true,
            block_sensitive_patterns: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub behavior: BehaviorConfig,
    #[serde(default)]
    pub privacy: PrivacyConfig,
}

impl AppConfig {
    pub fn default_config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("ai-assistant-for-writing").join("config.toml"))
    }

    pub fn load(explicit_path: Option<&Path>) -> Result<Self> {
        let mut cfg = if let Some(path) = explicit_path {
            Self::from_file(path)?
        } else if Path::new("config.local.toml").exists() {
            Self::from_file(Path::new("config.local.toml"))?
        } else if let Some(user_path) = Self::default_config_path().filter(|p| p.exists()) {
            Self::from_file(&user_path)?
        } else {
            Self::default()
        };

        cfg.apply_env_overrides();
        Ok(cfg)
    }

    fn from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("Failed to parse TOML config: {}", path.display()))
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(key) = std::env::var("COREBOX_API_KEY") {
            if !key.trim().is_empty() {
                self.provider.kind = ProviderKind::Corebox;
                self.provider.api_key = Some(key);
            }
        }
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            if !key.trim().is_empty() && self.provider.api_key.is_none() {
                self.provider.kind = ProviderKind::OpenaiCompatible;
                self.provider.api_key = Some(key);
            }
        }
        if let Ok(key) = std::env::var("GEMINI_API_KEY") {
            if !key.trim().is_empty() && self.provider.api_key.is_none() {
                self.provider.kind = ProviderKind::Gemini;
                self.provider.api_key = Some(key);
            }
        }
        if let Ok(url) = std::env::var("OLLAMA_URL") {
            if !url.trim().is_empty() {
                self.provider.base_url = Some(url);
            }
        }
        if let Ok(model) = std::env::var("AI_WRITING_MODEL") {
            if !model.trim().is_empty() {
                self.provider.autocomplete_model = Some(model);
            }
        }
        if let Ok(provider) = std::env::var("AI_WRITING_PROVIDER") {
            match provider.trim().to_lowercase().as_str() {
                "corebox" => self.provider.kind = ProviderKind::Corebox,
                "openai" | "openai-compatible" => {
                    self.provider.kind = ProviderKind::OpenaiCompatible
                }
                "gemini" => self.provider.kind = ProviderKind::Gemini,
                "ollama" => self.provider.kind = ProviderKind::Ollama,
                "mock" => self.provider.kind = ProviderKind::Mock,
                _ => {}
            }
        }
    }
}
