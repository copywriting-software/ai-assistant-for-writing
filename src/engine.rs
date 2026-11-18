use crate::ai::{AiProvider, GenerationRequest};
use crate::config::AppConfig;
use crate::encoding::{convert_en_to_ru_layout, looks_like_russian_in_en_layout};
use crate::memory::MemoryStore;
use crate::privacy::{PrivacyGuard, PrivacyVerdict};
use anyhow::{anyhow, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, Default)]
pub struct CompletionContext<'a> {
    pub app_name: Option<&'a str>,
    pub window_title: Option<&'a str>,
    pub text_after_cursor: Option<&'a str>,
    pub is_secure_field: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteStyle {
    Concise,
    Professional,
    Friendly,
    Clear,
}

impl RewriteStyle {
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "professional" | "formal" => Self::Professional,
            "friendly" | "casual" => Self::Friendly,
            "clear" => Self::Clear,
            _ => Self::Concise,
        }
    }

    fn instruction(&self) -> &'static str {
        match self {
            Self::Concise => "Make the text more concise while preserving all facts, numbers, names, and language.",
            Self::Professional => "Rewrite in a clear, polite, professional tone in the same language, preserving all facts and numbers.",
            Self::Friendly => "Rewrite in a warm, natural, friendly tone in the same language, preserving all facts and numbers.",
            Self::Clear => "Improve clarity and flow while preserving the exact meaning, language, numbers, and names.",
        }
    }
}

struct PrefixCache {
    capacity: usize,
    map: HashMap<String, String>,
    order: VecDeque<String>,
}

impl PrefixCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &str) -> Option<String> {
        if let Some(exact) = self.map.get(key) {
            return Some(exact.clone());
        }
        // Speculative prefix-continuation hit:
        // 1) Direct prefix extension (key = cached_prefix + typed)
        // 2) Sliding-window extension when prefix > max_context_chars
        for cached_prefix in self.order.iter().rev() {
            let Some(cached_sug) = self.map.get(cached_prefix) else {
                continue;
            };
            if let Some(rest) = key.strip_prefix(cached_prefix.as_str()) {
                if !rest.is_empty() {
                    if let Some(remaining) = cached_sug.strip_prefix(rest) {
                        if !remaining.is_empty() {
                            return Some(remaining.to_string());
                        }
                    }
                }
                continue;
            }

            // Sliding-window match: check if `key` ends with `<tail of cached_prefix><typed_prefix_of_sug>`
            let max_typed = cached_sug.len().min(32);
            for (byte_idx, _) in cached_sug.char_indices().skip(1) {
                if byte_idx > max_typed {
                    break;
                }
                let typed = &cached_sug[..byte_idx];
                if let Some(before_typed) = key.strip_suffix(typed) {
                    if before_typed.len() >= 24 && cached_prefix.ends_with(before_typed) {
                        let remaining = &cached_sug[byte_idx..];
                        if !remaining.is_empty() {
                            return Some(remaining.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    fn insert(&mut self, key: String, val: String) {
        if self.map.contains_key(&key) {
            self.map.insert(key, val);
            return;
        }
        if self.order.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, val);
    }
}

pub struct WritingEngine {
    config: AppConfig,
    provider: Arc<dyn AiProvider>,
    privacy: PrivacyGuard,
    cache: Mutex<PrefixCache>,
    memory: Arc<MemoryStore>,
}

impl WritingEngine {
    pub fn new(config: AppConfig, provider: Arc<dyn AiProvider>) -> Self {
        Self::with_memory(config, provider, Arc::new(MemoryStore::in_memory()))
    }

    pub fn with_memory(
        config: AppConfig,
        provider: Arc<dyn AiProvider>,
        memory: Arc<MemoryStore>,
    ) -> Self {
        let privacy = PrivacyGuard::new(config.privacy.clone());
        let cache = Mutex::new(PrefixCache::new(config.behavior.cache_size));
        Self {
            config,
            provider,
            privacy,
            cache,
            memory,
        }
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn provider(&self) -> &Arc<dyn AiProvider> {
        &self.provider
    }

    pub fn memory(&self) -> &Arc<MemoryStore> {
        &self.memory
    }

    pub fn record_accept(
        &self,
        app_name: Option<&str>,
        is_secure_field: bool,
        prefix: &str,
        completion: &str,
        word_only: bool,
    ) {
        if self.privacy.check(app_name, is_secure_field, prefix) == PrivacyVerdict::Allowed {
            self.memory
                .record_accept(app_name, prefix, completion, word_only);
        }
    }

    pub fn record_dismiss(&self, app_name: Option<&str>, is_secure_field: bool) {
        if self.privacy.check(app_name, is_secure_field, "") == PrivacyVerdict::Allowed {
            self.memory.record_dismiss(app_name);
        }
    }

    /// Generates an inline continuation suffix for the text before the cursor.
    /// Returns `Ok(None)` if blocked by privacy, too short, or no useful completion.
    pub async fn complete(
        &self,
        prefix: &str,
        app_name: Option<&str>,
        is_secure_field: bool,
        cancel: CancellationToken,
    ) -> Result<Option<String>> {
        self.complete_with_context(
            prefix,
            CompletionContext {
                app_name,
                is_secure_field,
                ..Default::default()
            },
            cancel,
        )
        .await
    }

    /// Context-aware inline completion using app/window metadata, text after cursor (FIM),
    /// sliding-window prefix cache, and 0ms learned fast-path memory.
    pub async fn complete_with_context(
        &self,
        prefix: &str,
        ctx: CompletionContext<'_>,
        cancel: CancellationToken,
    ) -> Result<Option<String>> {
        if let PrivacyVerdict::Blocked(reason) =
            self.privacy
                .check(ctx.app_name, ctx.is_secure_field, prefix)
        {
            tracing::debug!("Privacy guard blocked completion: {}", reason);
            return Ok(None);
        }

        let trimmed_len = prefix.trim().chars().count();
        if trimmed_len < self.config.behavior.min_prefix_chars {
            return Ok(None);
        }

        // Trim context to the last N chars to keep latency low.
        let context = tail_chars(prefix, self.config.behavior.max_context_chars);

        if let Some(cached) = self.cache.lock().unwrap().get(&context) {
            self.memory.record_request(ctx.app_name, None, true, false);
            return Ok(Some(cached));
        }

        if let Some(fast) = self.memory.fast_path_match(&context, ctx.app_name) {
            self.memory.record_request(ctx.app_name, None, false, true);
            return Ok(Some(fast));
        }

        let mut system_prompt = String::from(
            "\
You are an ultra-fast inline writing autocomplete engine.
Continue the user's text naturally from the exact cursor position.
Rules:
- Output ONLY the continuation suffix to append directly after the user's text.
- Do NOT repeat any part of the user's input or text after the cursor.
- Match the user's language (Russian, English, etc.) and tone automatically.
- Keep it short: 3 to 15 words, finishing the current thought or sentence cleanly.
- Never wrap in quotes, markdown, or explanations.
- If the user's text ends without a trailing space and the next word needs one, start with a single space.",
        );

        if let Some(app) = ctx.app_name.filter(|s| !s.trim().is_empty()) {
            system_prompt.push_str(&format!("\n- Active application: {}", app));
        }
        if let Some(title) = ctx.window_title.filter(|s| !s.trim().is_empty()) {
            let short_title = tail_chars(title, 80);
            system_prompt.push_str(&format!("\n- Window title: {}", short_title));
        }
        if let Some(hints) = self.memory.style_hints(ctx.app_name) {
            system_prompt.push_str(&format!("\n- {}", hints));
        }

        let after_trimmed = ctx
            .text_after_cursor
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let user_prompt = match after_trimmed {
            Some(after) => format!(
                "Continue from [CURSOR] without repeating text after it:\n{}[CURSOR]{}",
                context,
                tail_chars(after, 200)
            ),
            None => format!("Continue from the exact end of this text:\n{}", context),
        };

        let started = Instant::now();
        let raw = self
            .provider
            .generate(
                GenerationRequest {
                    system_prompt,
                    user_prompt,
                    model: self.config.provider.effective_autocomplete_model(),
                    max_tokens: self.config.behavior.max_completion_tokens,
                    temperature: 0.25,
                },
                cancel,
            )
            .await?;
        let latency_ms = started.elapsed().as_millis() as u64;
        self.memory
            .record_request(ctx.app_name, Some(latency_ms), false, false);

        let cleaned = sanitize_completion(&context, &raw);
        if cleaned.is_empty() {
            return Ok(None);
        }

        self.cache.lock().unwrap().insert(context, cleaned.clone());
        Ok(Some(cleaned))
    }

    /// Fixes grammar, spelling, punctuation, and broken keyboard layout / encoding.
    pub async fn fix_grammar(
        &self,
        text: &str,
        app_name: Option<&str>,
        is_secure_field: bool,
        cancel: CancellationToken,
    ) -> Result<String> {
        if let PrivacyVerdict::Blocked(reason) = self.privacy.check(app_name, is_secure_field, text) {
            return Err(anyhow!("Blocked by privacy guard: {}", reason));
        }

        // Fast deterministic layout repair if clearly Russian typed on EN layout.
        let pre_repaired = if looks_like_russian_in_en_layout(text) {
            convert_en_to_ru_layout(text)
        } else {
            text.to_string()
        };

        let system_prompt = "\
You are a precise proofreading assistant.
Fix spelling, punctuation, grammar, and any broken keyboard layout or character encoding (mojibake) if present.
Rules:
- Preserve the original meaning, tone, names, numbers, URLs, and formatting.
- Keep the same language (unless fixing wrong-layout/garbled text).
- Output ONLY the corrected text, with no commentary, quotes, or markdown fences.";

        let out = self
            .provider
            .generate(
                GenerationRequest {
                    system_prompt: system_prompt.to_string(),
                    user_prompt: pre_repaired,
                    model: self.config.provider.effective_edit_model(),
                    max_tokens: 512,
                    temperature: 0.1,
                },
                cancel,
            )
            .await?;

        Ok(strip_code_fences(&out))
    }

    /// Rewrites selected text in the requested style.
    pub async fn rewrite(
        &self,
        text: &str,
        style: RewriteStyle,
        app_name: Option<&str>,
        is_secure_field: bool,
        cancel: CancellationToken,
    ) -> Result<String> {
        if let PrivacyVerdict::Blocked(reason) = self.privacy.check(app_name, is_secure_field, text) {
            return Err(anyhow!("Blocked by privacy guard: {}", reason));
        }

        let system_prompt = format!(
            "You are a writing assistant. {}\nOutput ONLY the rewritten text without commentary or quotes.",
            style.instruction()
        );

        let out = self
            .provider
            .generate(
                GenerationRequest {
                    system_prompt,
                    user_prompt: text.to_string(),
                    model: self.config.provider.effective_edit_model(),
                    max_tokens: 512,
                    temperature: 0.4,
                },
                cancel,
            )
            .await?;

        Ok(strip_code_fences(&out))
    }
}

/// Extracts the next word (including any leading whitespace) from a ghost suggestion
/// for word-by-word acceptance.
pub fn next_word_chunk(suggestion: &str) -> String {
    if suggestion.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let mut seen_non_ws = false;
    for g in suggestion.graphemes(true) {
        let is_ws = g.chars().all(|c| c.is_whitespace());
        if is_ws {
            if seen_non_ws {
                break;
            }
            out.push_str(g);
        } else {
            seen_non_ws = true;
            out.push_str(g);
        }
    }
    out
}

fn tail_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    s.chars().skip(count - max_chars).collect()
}

fn strip_code_fences(s: &str) -> String {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or(rest);
        return rest.trim_end_matches("```").trim().to_string();
    }
    t.to_string()
}

/// Cleans model output so it can be seamlessly appended after `prefix`.
pub fn sanitize_completion(prefix: &str, raw: &str) -> String {
    let mut s = strip_code_fences(raw);
    // Take only the first line/paragraph for inline ghost text
    if let Some((first_line, _)) = s.split_once('\n') {
        s = first_line.to_string();
    }
    // Strip surrounding quotes if the model wrapped the output
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('«') && s.ends_with('»')) {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() >= 2 {
            s = chars[1..chars.len() - 1].iter().collect();
        }
    }

    // If the model echoed the tail of the prefix, strip the overlap
    let prefix_trimmed = prefix.trim_end();
    if !prefix_trimmed.is_empty() {
        if let Some(idx) = s.find(prefix_trimmed) {
            if idx == 0 {
                s = s[prefix_trimmed.len()..].to_string();
            }
        } else {
            // Check word-level suffix/prefix overlap (e.g. prefix ends with "поводу", output starts with "поводу встречи")
            if let Some(last_word) = prefix_trimmed.split_whitespace().last() {
                let s_trim = s.trim_start();
                if let Some(rest) = s_trim.strip_prefix(last_word) {
                    if rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with(',') || rest.starts_with('.') {
                        s = rest.to_string();
                    }
                }
            }
        }
    }

    // Normalize leading space relative to `prefix`
    let prefix_ends_ws = prefix.chars().last().map(|c| c.is_whitespace()).unwrap_or(false);
    if prefix_ends_ws {
        s = s.trim_start().to_string();
    } else if !s.is_empty() {
        let first = s.chars().next().unwrap();
        let needs_space = !matches!(first, ',' | '.' | '!' | '?' | ':' | ';' | ')' | '»' | '-')
            && !first.is_whitespace();
        if needs_space {
            s.insert(0, ' ');
        }
    }

    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_overlap_and_fixes_spacing() {
        assert_eq!(
            sanitize_completion("Привет, хотел уточнить по поводу", "поводу завтрашней встречи."),
            " завтрашней встречи."
        );
        assert_eq!(
            sanitize_completion("Привет, хотел уточнить по поводу ", "завтрашней встречи."),
            "завтрашней встречи."
        );
        assert_eq!(
            sanitize_completion("Hello world", ", how are you?"),
            ", how are you?"
        );
    }

    #[test]
    fn next_word_extracts_cleanly() {
        assert_eq!(next_word_chunk(" завтрашней встречи."), " завтрашней");
        assert_eq!(next_word_chunk("встречи."), "встречи.");
        assert_eq!(next_word_chunk(""), "");
    }

    #[test]
    fn prefix_cache_serves_continuation_locally() {
        let mut cache = PrefixCache::new(16);
        cache.insert(
            "Привет, хотел уточнить по поводу".to_string(),
            " завтрашней встречи.".to_string(),
        );
        assert_eq!(
            cache.get("Привет, хотел уточнить по поводу зав"),
            Some("трашней встречи.".to_string())
        );
        assert_eq!(
            cache.get("Привет, хотел уточнить по поводу другого"),
            None
        );
    }

    #[test]
    fn prefix_cache_handles_sliding_window_shift() {
        let mut cache = PrefixCache::new(16);
        let base = "Очень длинный контекст документа, который превышает лимит окна и сдвигается влево";
        cache.insert(base.to_string(), " прямо сейчас.".to_string());

        // Simulate window shift: drop first 10 chars and append " прям"
        let shifted: String = base.chars().skip(10).collect::<String>() + " прям";
        assert_eq!(cache.get(&shifted), Some("о сейчас.".to_string()));
    }
}
