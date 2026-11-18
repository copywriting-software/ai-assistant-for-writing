use crate::engine::{next_word_chunk, CompletionContext, RewriteStyle, WritingEngine};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CursorRect {
    pub x: f64,
    pub y: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FocusSnapshot {
    pub app_name: Option<String>,
    #[serde(default)]
    pub window_title: Option<String>,
    pub is_secure_field: bool,
    pub text_before_cursor: String,
    #[serde(default)]
    pub text_after_cursor: Option<String>,
    pub selected_text: Option<String>,
    pub cursor_rect: Option<CursorRect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSuggestion {
    pub expected_prefix: String,
    pub suggestion: String,
    pub revision: u64,
    pub app_name: Option<String>,
    pub is_secure_field: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionAction {
    ShowThinking {
        rect: Option<CursorRect>,
    },
    ShowGhost {
        text: String,
        rect: Option<CursorRect>,
    },
    HideGhost,
    InsertText {
        text: String,
        expected_prefix: String,
    },
    ReplaceSelection {
        text: String,
    },
    NotifyError(String),
}

pub struct SessionManager {
    engine: Arc<WritingEngine>,
    state: Arc<Mutex<SessionState>>,
}

struct SessionState {
    revision: u64,
    in_flight: Option<CancellationToken>,
    active: Option<ActiveSuggestion>,
    dismissed_prefix: Option<String>,
    last_app_name: Option<String>,
    last_is_secure: bool,
}

impl SessionManager {
    pub fn new(engine: Arc<WritingEngine>) -> Self {
        Self {
            engine,
            state: Arc::new(Mutex::new(SessionState {
                revision: 0,
                in_flight: None,
                active: None,
                dismissed_prefix: None,
                last_app_name: None,
                last_is_secure: false,
            })),
        }
    }

    /// Cancels any previous in-flight request, clears stale ghost text, and schedules an adaptive debounced completion.
    pub async fn on_text_changed<F>(&self, snapshot: FocusSnapshot, emit: F)
    where
        F: Fn(SessionAction) + Send + Sync + 'static,
    {
        let (rev, token, suppressed) = {
            let mut st = self.state.lock().await;
            st.revision = st.revision.wrapping_add(1);
            if let Some(prev) = st.in_flight.take() {
                prev.cancel();
            }
            st.active = None;
            st.last_app_name = snapshot.app_name.clone();
            st.last_is_secure = snapshot.is_secure_field;

            let suppressed = if let Some(dismissed) = &st.dismissed_prefix {
                if let Some(added) = snapshot.text_before_cursor.strip_prefix(dismissed.as_str()) {
                    // Keep suppressed until user types whitespace/newline (new word)
                    !added.chars().any(|c| c.is_whitespace())
                } else {
                    st.dismissed_prefix = None;
                    false
                }
            } else {
                false
            };

            let token = CancellationToken::new();
            st.in_flight = Some(token.clone());
            (st.revision, token, suppressed)
        };

        emit(SessionAction::HideGhost);

        if suppressed {
            return;
        }

        let trimmed_len = snapshot.text_before_cursor.trim().chars().count();
        if trimmed_len < self.engine.config().behavior.min_prefix_chars {
            return;
        }

        let engine = Arc::clone(&self.engine);
        let state = Arc::clone(&self.state);
        let base_debounce = engine.config().behavior.debounce_ms;
        let debounce = std::time::Duration::from_millis(adaptive_debounce_ms(
            &snapshot.text_before_cursor,
            base_debounce,
        ));

        tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => return,
                _ = tokio::time::sleep(debounce) => {}
            }

            if token.is_cancelled() {
                return;
            }

            emit(SessionAction::ShowThinking {
                rect: snapshot.cursor_rect.clone(),
            });

            let res = engine
                .complete_with_context(
                    &snapshot.text_before_cursor,
                    CompletionContext {
                        app_name: snapshot.app_name.as_deref(),
                        window_title: snapshot.window_title.as_deref(),
                        text_after_cursor: snapshot.text_after_cursor.as_deref(),
                        is_secure_field: snapshot.is_secure_field,
                    },
                    token.clone(),
                )
                .await;

            if token.is_cancelled() {
                return;
            }

            match res {
                Ok(Some(suggestion)) => {
                    let mut st = state.lock().await;
                    if st.revision != rev {
                        return;
                    }
                    st.active = Some(ActiveSuggestion {
                        expected_prefix: snapshot.text_before_cursor,
                        suggestion: suggestion.clone(),
                        revision: rev,
                        app_name: snapshot.app_name,
                        is_secure_field: snapshot.is_secure_field,
                    });
                    emit(SessionAction::ShowGhost {
                        text: suggestion,
                        rect: snapshot.cursor_rect,
                    });
                }
                Ok(None) => {
                    emit(SessionAction::HideGhost);
                }
                Err(err) => {
                    if !token.is_cancelled() {
                        tracing::warn!("Completion error: {}", err);
                    }
                    emit(SessionAction::HideGhost);
                }
            }
        });
    }

    pub async fn accept_all(&self) -> Option<SessionAction> {
        let mut st = self.state.lock().await;
        let active = st.active.take()?;
        st.dismissed_prefix = None;
        self.engine.record_accept(
            active.app_name.as_deref(),
            active.is_secure_field,
            &active.expected_prefix,
            &active.suggestion,
            false,
        );
        Some(SessionAction::InsertText {
            text: active.suggestion,
            expected_prefix: active.expected_prefix,
        })
    }

    pub async fn accept_next_word(&self) -> Vec<SessionAction> {
        let mut st = self.state.lock().await;
        let Some(active) = st.active.take() else {
            return vec![];
        };

        let chunk = next_word_chunk(&active.suggestion);
        if chunk.is_empty() {
            return vec![SessionAction::HideGhost];
        }

        self.engine.record_accept(
            active.app_name.as_deref(),
            active.is_secure_field,
            &active.expected_prefix,
            &chunk,
            true,
        );

        let remaining = active.suggestion[chunk.len()..].to_string();
        let insert = SessionAction::InsertText {
            text: chunk.clone(),
            expected_prefix: active.expected_prefix.clone(),
        };

        if remaining.is_empty() {
            vec![insert, SessionAction::HideGhost]
        } else {
            let new_prefix = format!("{}{}", active.expected_prefix, chunk);
            st.active = Some(ActiveSuggestion {
                expected_prefix: new_prefix,
                suggestion: remaining.clone(),
                revision: active.revision,
                app_name: active.app_name,
                is_secure_field: active.is_secure_field,
            });
            vec![
                insert,
                SessionAction::ShowGhost {
                    text: remaining,
                    rect: None,
                },
            ]
        }
    }

    pub async fn dismiss(&self) -> SessionAction {
        let mut st = self.state.lock().await;
        if let Some(prev) = st.in_flight.take() {
            prev.cancel();
        }
        if let Some(active) = st.active.take() {
            st.dismissed_prefix = Some(active.expected_prefix);
            self.engine
                .record_dismiss(active.app_name.as_deref(), active.is_secure_field);
        } else {
            self.engine
                .record_dismiss(st.last_app_name.as_deref(), st.last_is_secure);
        }
        SessionAction::HideGhost
    }

    pub async fn fix_selection(&self, snapshot: FocusSnapshot) -> SessionAction {
        let target = snapshot
            .selected_text
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&snapshot.text_before_cursor);

        if target.trim().is_empty() {
            return SessionAction::HideGhost;
        }

        let cancel = {
            let mut st = self.state.lock().await;
            if let Some(prev) = st.in_flight.take() {
                prev.cancel();
            }
            let token = CancellationToken::new();
            st.in_flight = Some(token.clone());
            token
        };
        match self
            .engine
            .fix_grammar(
                target,
                snapshot.app_name.as_deref(),
                snapshot.is_secure_field,
                cancel.clone(),
            )
            .await
        {
            Ok(fixed) if !cancel.is_cancelled() => SessionAction::ReplaceSelection { text: fixed },
            Ok(_) => SessionAction::HideGhost,
            Err(_) if cancel.is_cancelled() => SessionAction::HideGhost,
            Err(e) => SessionAction::NotifyError(e.to_string()),
        }
    }

    pub async fn rewrite_selection(&self, snapshot: FocusSnapshot, style: RewriteStyle) -> SessionAction {
        let target = snapshot
            .selected_text
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&snapshot.text_before_cursor);

        if target.trim().is_empty() {
            return SessionAction::HideGhost;
        }

        let cancel = {
            let mut st = self.state.lock().await;
            if let Some(prev) = st.in_flight.take() {
                prev.cancel();
            }
            let token = CancellationToken::new();
            st.in_flight = Some(token.clone());
            token
        };
        match self
            .engine
            .rewrite(
                target,
                style,
                snapshot.app_name.as_deref(),
                snapshot.is_secure_field,
                cancel.clone(),
            )
            .await
        {
            Ok(rewritten) if !cancel.is_cancelled() => SessionAction::ReplaceSelection { text: rewritten },
            Ok(_) => SessionAction::HideGhost,
            Err(_) if cancel.is_cancelled() => SessionAction::HideGhost,
            Err(e) => SessionAction::NotifyError(e.to_string()),
        }
    }
}

/// Computes an adaptive debounce: faster after word boundaries/punctuation,
/// full configured delay mid-word to avoid interrupting active typing.
pub fn adaptive_debounce_ms(text_before_cursor: &str, base_ms: u64) -> u64 {
    let Some(last) = text_before_cursor.chars().last() else {
        return base_ms;
    };
    if last.is_whitespace() || matches!(last, '.' | ',' | '!' | '?' | ':' | ';') {
        (base_ms * 7 / 10).max(80).min(base_ms)
    } else {
        base_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiProvider, GenerationRequest};
    use crate::config::AppConfig;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct SlowMockProvider {
        calls: AtomicUsize,
        delay_ms: u64,
    }

    #[async_trait]
    impl AiProvider for SlowMockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }
        async fn generate(&self, _req: GenerationRequest, cancel: CancellationToken) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::select! {
                _ = cancel.cancelled() => Err(anyhow::anyhow!("cancelled")),
                _ = tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)) => {
                    Ok(" завтрашней встречи в 15:00.".to_string())
                }
            }
        }
        async fn health_check(&self) -> Result<String> {
            Ok("mock ok".to_string())
        }
    }

    #[tokio::test]
    async fn cancels_stale_requests_and_supports_word_accept() {
        let mut cfg = AppConfig::default();
        cfg.behavior.debounce_ms = 20;
        let provider = Arc::new(SlowMockProvider {
            calls: AtomicUsize::new(0),
            delay_ms: 15,
        });
        let engine = Arc::new(WritingEngine::new(cfg, provider));
        let session = SessionManager::new(engine);

        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);

        session
            .on_text_changed(
                FocusSnapshot {
                    app_name: Some("TextEdit".into()),
                    is_secure_field: false,
                    text_before_cursor: "Привет, хотел".into(),
                    ..Default::default()
                },
                move |a| {
                    let ev = Arc::clone(&events_clone);
                    tokio::spawn(async move {
                        ev.lock().await.push(a);
                    });
                },
            )
            .await;

        // Quickly supersede with a newer snapshot
        let events_clone2 = Arc::clone(&events);
        session
            .on_text_changed(
                FocusSnapshot {
                    app_name: Some("TextEdit".into()),
                    is_secure_field: false,
                    text_before_cursor: "Привет, хотел уточнить по поводу".into(),
                    ..Default::default()
                },
                move |a| {
                    let ev = Arc::clone(&events_clone2);
                    tokio::spawn(async move {
                        ev.lock().await.push(a);
                    });
                },
            )
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let actions = session.accept_next_word().await;
        assert!(matches!(
            actions.first(),
            Some(SessionAction::InsertText { text, .. }) if text == " завтрашней"
        ));
    }

    #[test]
    fn adaptive_debounce_is_faster_at_word_boundaries() {
        assert_eq!(adaptive_debounce_ms("Привет, ", 300), 210);
        assert_eq!(adaptive_debounce_ms("Привет", 300), 300);
    }
}
