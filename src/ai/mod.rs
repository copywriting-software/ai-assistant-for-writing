use crate::config::{ProviderConfig, ProviderKind};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct GenerationRequest {
    pub system_prompt: String,
    pub user_prompt: String,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

#[async_trait]
pub trait AiProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn generate(&self, req: GenerationRequest, cancel: CancellationToken) -> Result<String>;
    async fn health_check(&self) -> Result<String>;
}

pub fn create_provider(cfg: &ProviderConfig) -> Result<Arc<dyn AiProvider>> {
    let client = Client::builder()
        .timeout(Duration::from_millis(cfg.timeout_ms))
        .build()
        .context("Failed to build HTTP client")?;

    match cfg.kind {
        ProviderKind::Corebox | ProviderKind::OpenaiCompatible => {
            let api_key = cfg
                .api_key
                .clone()
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "API key is required for {:?} provider. Set COREBOX_API_KEY / OPENAI_API_KEY or configure api_key in config.toml",
                        cfg.kind
                    )
                })?;
            Ok(Arc::new(OpenAiCompatibleProvider {
                client,
                base_url: cfg.effective_base_url(),
                api_key,
                provider_label: if cfg.kind == ProviderKind::Corebox {
                    "corebox"
                } else {
                    "openai-compatible"
                },
            }))
        }
        ProviderKind::Ollama => Ok(Arc::new(OllamaProvider {
            client,
            base_url: cfg.effective_base_url(),
        })),
        ProviderKind::Gemini => {
            let api_key = cfg
                .api_key
                .clone()
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| anyhow!("GEMINI_API_KEY or api_key is required for Gemini provider"))?;
            Ok(Arc::new(GeminiProvider {
                client,
                base_url: cfg.effective_base_url(),
                api_key,
            }))
        }
        ProviderKind::Mock => Ok(Arc::new(MockProvider)),
    }
}

/// Offline deterministic mock provider for testing and local demos without a server.
pub struct MockProvider;

#[async_trait]
impl AiProvider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn generate(&self, req: GenerationRequest, cancel: CancellationToken) -> Result<String> {
        if cancel.is_cancelled() {
            return Err(anyhow!("cancelled"));
        }
        let sys = req.system_prompt.to_lowercase();
        if sys.contains("proofreading") {
            let trimmed = req.user_prompt.trim();
            if trimmed.eq_ignore_ascii_case("привет как дела") {
                return Ok("Привет, как дела?".to_string());
            }
            return Ok(format!("{} (исправлено)", trimmed));
        }
        if sys.contains("writing assistant") {
            return Ok(format!("[concise] {}", req.user_prompt.trim()));
        }

        // Autocomplete mode: match Russian vs English
        let is_cyrillic = req
            .user_prompt
            .chars()
            .any(|c| ('\u{0400}'..='\u{04FF}').contains(&c));
        if is_cyrillic {
            Ok(" нашей встречи, которую мы обсуждали на прошлой неделе.".to_string())
        } else {
            Ok(" our meeting scheduled for tomorrow afternoon.".to_string())
        }
    }

    async fn health_check(&self) -> Result<String> {
        Ok("mock OK (offline)".to_string())
    }
}

pub struct OpenAiCompatibleProvider {
    client: Client,
    base_url: String,
    api_key: String,
    provider_label: &'static str,
}

#[derive(Serialize)]
struct OpenAiChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage<'a>>,
    max_tokens: u32,
    temperature: f32,
    stream: bool,
}

#[derive(Serialize)]
struct OpenAiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessageOut,
}

#[derive(Deserialize)]
struct OpenAiMessageOut {
    content: Option<String>,
}

#[async_trait]
impl AiProvider for OpenAiCompatibleProvider {
    fn name(&self) -> &'static str {
        self.provider_label
    }

    async fn generate(&self, req: GenerationRequest, cancel: CancellationToken) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = OpenAiChatRequest {
            model: &req.model,
            messages: vec![
                OpenAiMessage {
                    role: "system",
                    content: &req.system_prompt,
                },
                OpenAiMessage {
                    role: "user",
                    content: &req.user_prompt,
                },
            ],
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            stream: false,
        };

        let fut = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send();

        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(anyhow!("cancelled")),
            res = fut => res.with_context(|| format!("Request to {} failed", url))?,
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("API error {}: {}", status, text));
        }

        let parsed: OpenAiChatResponse = resp
            .json()
            .await
            .context("Failed to parse chat completions response")?;

        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();

        Ok(content)
    }

    async fn health_check(&self) -> Result<String> {
        let url = format!("{}/models", self.base_url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .with_context(|| format!("Failed to reach {}", url))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Health check failed ({}): {}", status, text));
        }
        Ok(format!("{} OK ({})", self.provider_label, self.base_url))
    }
}

pub struct OllamaProvider {
    client: Client,
    base_url: String,
}

#[derive(Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage<'a>>,
    stream: bool,
    options: OllamaOptions,
}

#[derive(Serialize)]
struct OllamaOptions {
    num_predict: u32,
    temperature: f32,
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: Option<OpenAiMessageOut>,
}

#[async_trait]
impl AiProvider for OllamaProvider {
    fn name(&self) -> &'static str {
        "ollama"
    }

    async fn generate(&self, req: GenerationRequest, cancel: CancellationToken) -> Result<String> {
        let url = format!("{}/api/chat", self.base_url);
        let body = OllamaChatRequest {
            model: &req.model,
            messages: vec![
                OpenAiMessage {
                    role: "system",
                    content: &req.system_prompt,
                },
                OpenAiMessage {
                    role: "user",
                    content: &req.user_prompt,
                },
            ],
            stream: false,
            options: OllamaOptions {
                num_predict: req.max_tokens,
                temperature: req.temperature,
            },
        };

        let fut = self.client.post(&url).json(&body).send();
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(anyhow!("cancelled")),
            res = fut => res.with_context(|| format!("Request to Ollama {} failed", url))?,
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Ollama error {}: {}", status, text));
        }

        let parsed: OllamaChatResponse = resp
            .json()
            .await
            .context("Failed to parse Ollama response")?;

        Ok(parsed.message.and_then(|m| m.content).unwrap_or_default())
    }

    async fn health_check(&self) -> Result<String> {
        let url = format!("{}/api/tags", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Ollama not reachable at {}", self.base_url))?;
        if !resp.status().is_success() {
            return Err(anyhow!("Ollama returned {}", resp.status()));
        }
        Ok(format!("ollama OK ({})", self.base_url))
    }
}

pub struct GeminiProvider {
    client: Client,
    base_url: String,
    api_key: String,
}

#[derive(Serialize)]
struct GeminiRequest<'a> {
    #[serde(rename = "systemInstruction")]
    system_instruction: GeminiContent<'a>,
    contents: Vec<GeminiContent<'a>>,
    #[serde(rename = "generationConfig")]
    generation_config: GeminiGenConfig,
}

#[derive(Serialize)]
struct GeminiContent<'a> {
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Serialize)]
struct GeminiPart<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct GeminiGenConfig {
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
    temperature: f32,
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiContentOut>,
}

#[derive(Deserialize)]
struct GeminiContentOut {
    parts: Option<Vec<GeminiPartOut>>,
}

#[derive(Deserialize)]
struct GeminiPartOut {
    text: Option<String>,
}

#[async_trait]
impl AiProvider for GeminiProvider {
    fn name(&self) -> &'static str {
        "gemini"
    }

    async fn generate(&self, req: GenerationRequest, cancel: CancellationToken) -> Result<String> {
        let url = format!(
            "{}/models/{}:generateContent?key={}",
            self.base_url, req.model, self.api_key
        );
        let body = GeminiRequest {
            system_instruction: GeminiContent {
                parts: vec![GeminiPart {
                    text: &req.system_prompt,
                }],
            },
            contents: vec![GeminiContent {
                parts: vec![GeminiPart {
                    text: &req.user_prompt,
                }],
            }],
            generation_config: GeminiGenConfig {
                max_output_tokens: req.max_tokens,
                temperature: req.temperature,
            },
        };

        let fut = self.client.post(&url).json(&body).send();
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(anyhow!("cancelled")),
            res = fut => res.context("Request to Gemini API failed")?,
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Gemini API error {}: {}", status, text));
        }

        let parsed: GeminiResponse = resp.json().await.context("Failed to parse Gemini response")?;
        let text = parsed
            .candidates
            .unwrap_or_default()
            .into_iter()
            .next()
            .and_then(|c| c.content)
            .and_then(|c| c.parts)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| p.text)
            .collect::<Vec<_>>()
            .join("");
        Ok(text)
    }

    async fn health_check(&self) -> Result<String> {
        let url = format!("{}/models?key={}", self.base_url, self.api_key);
        let resp = self.client.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("Gemini health check failed: {}", resp.status()));
        }
        Ok("gemini OK".to_string())
    }
}
