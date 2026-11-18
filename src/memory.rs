use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LEARNED_PHRASES: usize = 500;
const PREFIX_TAIL_CHARS: usize = 36;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppStats {
    pub requests: u64,
    pub accepted: u64,
    pub dismissed: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryStats {
    pub total_requests: u64,
    pub cache_hits: u64,
    pub fast_path_hits: u64,
    pub accepted_all: u64,
    pub accepted_words: u64,
    pub dismissed: u64,
    pub total_latency_ms: u64,
    pub latency_samples: u64,
    #[serde(default)]
    pub by_app: HashMap<String, AppStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnedPhrase {
    pub prefix_tail: String,
    pub completion: String,
    pub app: Option<String>,
    pub count: u32,
    pub last_used_epoch: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MemoryData {
    #[serde(default)]
    stats: MemoryStats,
    #[serde(default)]
    phrases: Vec<LearnedPhrase>,
}

/// Persistent local memory for accepted writing patterns, fast-path completions,
/// and latency/acceptance telemetry.
pub struct MemoryStore {
    path: Option<PathBuf>,
    data: Mutex<MemoryData>,
}

impl MemoryStore {
    pub fn default_path() -> Option<PathBuf> {
        dirs::data_local_dir().map(|d| d.join("ai-assistant-for-writing").join("memory.json"))
    }

    pub fn load_default() -> Self {
        match Self::default_path() {
            Some(path) => Self::load_from_path(path),
            None => Self::in_memory(),
        }
    }

    pub fn in_memory() -> Self {
        Self {
            path: None,
            data: Mutex::new(MemoryData::default()),
        }
    }

    pub fn load_from_path(path: PathBuf) -> Self {
        let data = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<MemoryData>(&raw).ok())
                .unwrap_or_default()
        } else {
            MemoryData::default()
        };
        Self {
            path: Some(path),
            data: Mutex::new(data),
        }
    }

    /// Returns a 0ms local suggestion if the user has repeatedly accepted a completion
    /// for the exact trailing context.
    pub fn fast_path_match(&self, prefix: &str, app_name: Option<&str>) -> Option<String> {
        let tail = tail_chars_normalized(prefix, PREFIX_TAIL_CHARS);
        if tail.chars().count() < 4 {
            return None;
        }

        let data = self.data.lock().unwrap();
        let app_lower = app_name.map(|a| a.to_lowercase());

        data.phrases
            .iter()
            .filter(|p| p.count >= 2 && tail.ends_with(&p.prefix_tail))
            .max_by_key(|p| {
                let app_bonus = match (&p.app, &app_lower) {
                    (Some(pa), Some(ca)) if pa.eq_ignore_ascii_case(ca) => 1000u32,
                    (None, _) => 100,
                    _ => 0,
                };
                (
                    app_bonus + p.count,
                    p.prefix_tail.chars().count() as u32,
                    p.last_used_epoch,
                )
            })
            .map(|p| p.completion.clone())
    }

    /// Returns a concise style/vocabulary hint for the system prompt based on past accepted text.
    pub fn style_hints(&self, app_name: Option<&str>) -> Option<String> {
        let data = self.data.lock().unwrap();
        let mut candidates: Vec<&LearnedPhrase> = data
            .phrases
            .iter()
            .filter(|p| match (app_name, &p.app) {
                (Some(cur), Some(saved)) => cur.eq_ignore_ascii_case(saved),
                _ => p.count >= 2,
            })
            .collect();

        if candidates.is_empty() {
            return None;
        }

        candidates.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| b.last_used_epoch.cmp(&a.last_used_epoch))
        });

        let samples: Vec<String> = candidates
            .into_iter()
            .take(4)
            .map(|p| format!("\"{}\"", p.completion.trim()))
            .filter(|s| s.len() > 2)
            .collect();

        if samples.is_empty() {
            None
        } else {
            Some(format!(
                "User's typical phrasing examples: {}",
                samples.join(", ")
            ))
        }
    }

    pub fn record_request(
        &self,
        app_name: Option<&str>,
        latency_ms: Option<u64>,
        cache_hit: bool,
        fast_path_hit: bool,
    ) {
        let mut data = self.data.lock().unwrap();
        data.stats.total_requests += 1;
        if cache_hit {
            data.stats.cache_hits += 1;
        }
        if fast_path_hit {
            data.stats.fast_path_hits += 1;
        }
        if let Some(ms) = latency_ms {
            data.stats.total_latency_ms += ms;
            data.stats.latency_samples += 1;
        }
        if let Some(app) = app_name {
            let entry = data.stats.by_app.entry(app.to_string()).or_default();
            entry.requests += 1;
        }
    }

    pub fn record_accept(
        &self,
        app_name: Option<&str>,
        prefix: &str,
        completion: &str,
        word_only: bool,
    ) {
        let mut data = self.data.lock().unwrap();
        if word_only {
            data.stats.accepted_words += 1;
        } else {
            data.stats.accepted_all += 1;
        }
        if let Some(app) = app_name {
            let entry = data.stats.by_app.entry(app.to_string()).or_default();
            entry.accepted += 1;
        }

        // Only learn full multi-char completions as fast-path phrases.
        if !word_only && completion.trim().chars().count() >= 4 {
            let tail = tail_chars_normalized(prefix, PREFIX_TAIL_CHARS);
            if tail.chars().count() >= 4 {
                let now = epoch_secs();
                if let Some(existing) = data.phrases.iter_mut().find(|p| {
                    p.prefix_tail == tail
                        && p.completion == completion
                        && p.app.as_deref() == app_name
                }) {
                    existing.count = existing.count.saturating_add(1);
                    existing.last_used_epoch = now;
                } else {
                    if data.phrases.len() >= MAX_LEARNED_PHRASES {
                        // Evict least-used / oldest phrase.
                        if let Some((idx, _)) = data
                            .phrases
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, p)| (p.count, p.last_used_epoch))
                        {
                            data.phrases.remove(idx);
                        }
                    }
                    data.phrases.push(LearnedPhrase {
                        prefix_tail: tail,
                        completion: completion.to_string(),
                        app: app_name.map(String::from),
                        count: 1,
                        last_used_epoch: now,
                    });
                }
            }
        }

        Self::save_locked(&self.path, &data);
    }

    pub fn record_dismiss(&self, app_name: Option<&str>) {
        let mut data = self.data.lock().unwrap();
        data.stats.dismissed += 1;
        if let Some(app) = app_name {
            let entry = data.stats.by_app.entry(app.to_string()).or_default();
            entry.dismissed += 1;
        }
        Self::save_locked(&self.path, &data);
    }

    pub fn flush(&self) {
        let data = self.data.lock().unwrap();
        Self::save_locked(&self.path, &data);
    }

    pub fn stats_snapshot(&self) -> MemoryStats {
        self.data.lock().unwrap().stats.clone()
    }

    pub fn summary_report(&self) -> String {
        let data = self.data.lock().unwrap();
        let s = &data.stats;
        let avg_latency = if s.latency_samples > 0 {
            s.total_latency_ms / s.latency_samples
        } else {
            0
        };
        let total_accepted = s.accepted_all + s.accepted_words;
        let decisions = total_accepted + s.dismissed;
        let accept_rate = if decisions > 0 {
            (total_accepted as f64 / decisions as f64) * 100.0
        } else {
            0.0
        };

        let mut lines = vec![
            format!("Total requests:   {}", s.total_requests),
            format!("Cache hits:       {}", s.cache_hits),
            format!("Fast-path hits:   {}", s.fast_path_hits),
            format!("Avg LLM latency:  {} ms", avg_latency),
            format!(
                "Accepted / Esc:   {} ({} full, {} word) / {} ({:.1}% accept)",
                total_accepted, s.accepted_all, s.accepted_words, s.dismissed, accept_rate
            ),
            format!("Learned phrases:  {}", data.phrases.len()),
        ];

        if !s.by_app.is_empty() {
            lines.push("By app:".to_string());
            let mut apps: Vec<(&String, &AppStats)> = s.by_app.iter().collect();
            apps.sort_by(|a, b| b.1.requests.cmp(&a.1.requests));
            for (app, st) in apps.into_iter().take(8) {
                lines.push(format!(
                    "  - {}: {} reqs, {} accepted, {} dismissed",
                    app, st.requests, st.accepted, st.dismissed
                ));
            }
        }

        lines.join("\n")
    }

    fn save_locked(path: &Option<PathBuf>, data: &MemoryData) {
        let Some(path) = path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(data) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}

fn tail_chars_normalized(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    let tail: String = if count <= max_chars {
        s.to_string()
    } else {
        s.chars().skip(count - max_chars).collect()
    };
    tail.to_lowercase()
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_path_learns_after_two_accepts_and_persists() {
        let tmp = std::env::temp_dir().join(format!("ai_writing_mem_{}.json", epoch_secs()));
        let _ = std::fs::remove_file(&tmp);

        {
            let store = MemoryStore::load_from_path(tmp.clone());
            assert_eq!(
                store.fast_path_match("Добрый день, ", Some("Mail")),
                None
            );
            store.record_accept(Some("Mail"), "Добрый день, ", "коллеги!", false);
            // Still requires >= 2 accepts to avoid noise
            assert_eq!(
                store.fast_path_match("Добрый день, ", Some("Mail")),
                None
            );
            store.record_accept(Some("Mail"), "Добрый день, ", "коллеги!", false);
            assert_eq!(
                store.fast_path_match("Привет! Добрый день, ", Some("Mail")),
                Some("коллеги!".to_string())
            );
        }

        // Reload from disk and verify persistence
        let reloaded = MemoryStore::load_from_path(tmp.clone());
        assert_eq!(
            reloaded.fast_path_match("Добрый день, ", Some("Mail")),
            Some("коллеги!".to_string())
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn stats_summary_formats_metrics() {
        let store = MemoryStore::in_memory();
        store.record_request(Some("Notes"), Some(120), false, false);
        store.record_request(Some("Notes"), None, true, false);
        store.record_accept(Some("Notes"), "Привет, ", "как дела?", false);
        store.record_dismiss(Some("Notes"));
        let report = store.summary_report();
        assert!(report.contains("Total requests:   2"));
        assert!(report.contains("Avg LLM latency:  120 ms"));
        assert!(report.contains("Notes: 2 reqs, 1 accepted, 1 dismissed"));
    }
}
