use ai_assistant_for_writing::ai::{create_provider, MockProvider};
use ai_assistant_for_writing::config::{AppConfig, ProviderKind};
use ai_assistant_for_writing::engine::{RewriteStyle, WritingEngine};
use ai_assistant_for_writing::session::{CursorRect, FocusSnapshot, SessionAction, SessionManager};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Simulates a native OS application text field (reading text, cursor rect, and applying InsertText/ReplaceSelection).
struct SimulatedAppWindow {
    app_name: String,
    is_secure_field: bool,
    buffer: String,
    selected_text: Option<String>,
    ghost_text: Option<String>,
    caret: CursorRect,
}

impl SimulatedAppWindow {
    fn new(app_name: &str, initial_text: &str) -> Self {
        Self {
            app_name: app_name.to_string(),
            is_secure_field: false,
            buffer: initial_text.to_string(),
            selected_text: None,
            ghost_text: None,
            caret: CursorRect {
                x: 240.0,
                y: 180.0,
                height: 18.0,
            },
        }
    }

    fn snapshot(&self) -> FocusSnapshot {
        FocusSnapshot {
            app_name: Some(self.app_name.clone()),
            is_secure_field: self.is_secure_field,
            text_before_cursor: self.buffer.clone(),
            selected_text: self.selected_text.clone(),
            cursor_rect: Some(self.caret.clone()),
            ..Default::default()
        }
    }

    /// Mirrors MacHelper.swift: applies SessionAction only if expected_prefix still matches.
    fn apply(&mut self, action: SessionAction) -> bool {
        match action {
            SessionAction::ShowThinking { .. } => true,
            SessionAction::ShowGhost { text, .. } => {
                self.ghost_text = Some(text);
                true
            }
            SessionAction::HideGhost => {
                self.ghost_text = None;
                true
            }
            SessionAction::InsertText {
                text,
                expected_prefix,
            } => {
                if self.buffer != expected_prefix {
                    // Safe abort: field changed before Tab was processed.
                    return false;
                }
                self.buffer.push_str(&text);
                self.ghost_text = None;
                true
            }
            SessionAction::ReplaceSelection { text } => {
                if let Some(sel) = &self.selected_text {
                    if let Some(pos) = self.buffer.rfind(sel) {
                        self.buffer.replace_range(pos..pos + sel.len(), &text);
                    }
                } else {
                    self.buffer = text;
                }
                self.selected_text = None;
                self.ghost_text = None;
                true
            }
            SessionAction::NotifyError(_) => false,
        }
    }
}

fn make_mock_session() -> Arc<SessionManager> {
    let mut cfg = AppConfig::default();
    cfg.provider.kind = ProviderKind::Mock;
    cfg.behavior.debounce_ms = 10;
    let provider = create_provider(&cfg.provider).expect("mock provider");
    let engine = Arc::new(WritingEngine::new(cfg, provider));
    Arc::new(SessionManager::new(engine))
}

#[tokio::test]
async fn matrix_autocomplete_and_insert_across_8_apps() {
    let apps = [
        ("TextEdit", "Привет, хотел уточнить по поводу", " нашей встречи, которую мы обсуждали на прошлой неделе."),
        ("Notes", "Привет, хотел уточнить по поводу", " нашей встречи, которую мы обсуждали на прошлой неделе."),
        ("Google Chrome", "Hello, I wanted to follow up on", " our meeting scheduled for tomorrow afternoon."),
        ("Telegram", "Привет, хотел уточнить по поводу", " нашей встречи, которую мы обсуждали на прошлой неделе."),
        ("Slack", "Hello, I wanted to follow up on", " our meeting scheduled for tomorrow afternoon."),
        ("Mail", "Привет, хотел уточнить по поводу", " нашей встречи, которую мы обсуждали на прошлой неделе."),
        ("Microsoft Word", "Hello, I wanted to follow up on", " our meeting scheduled for tomorrow afternoon."),
        ("Notion", "Привет, хотел уточнить по поводу", " нашей встречи, которую мы обсуждали на прошлой неделе."),
    ];

    let session = make_mock_session();

    for (app_name, prefix, expected_suffix) in apps {
        let window = Arc::new(Mutex::new(SimulatedAppWindow::new(app_name, prefix)));
        let snap = window.lock().await.snapshot();

        let win_clone = Arc::clone(&window);
        session
            .on_text_changed(snap, move |action| {
                let w = Arc::clone(&win_clone);
                tokio::spawn(async move {
                    w.lock().await.apply(action);
                });
            })
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(45)).await;

        {
            let w = window.lock().await;
            assert_eq!(
                w.ghost_text.as_deref(),
                Some(expected_suffix),
                "Ghost text failed for app {}",
                app_name
            );
        }

        // User presses Tab -> full suggestion inserted into app buffer
        let insert = session.accept_all().await.expect("active suggestion");
        let applied = window.lock().await.apply(insert);
        assert!(applied, "Insert should succeed in {}", app_name);
        assert_eq!(
            window.lock().await.buffer,
            format!("{}{}", prefix, expected_suffix)
        );
        assert!(window.lock().await.ghost_text.is_none());
    }
}

#[tokio::test]
async fn word_by_word_accept_and_dismiss_in_telegram() {
    let session = make_mock_session();
    let window = Arc::new(Mutex::new(SimulatedAppWindow::new(
        "Telegram",
        "Привет, хотел уточнить по поводу",
    )));
    let snap = window.lock().await.snapshot();

    let win_clone = Arc::clone(&window);
    session
        .on_text_changed(snap, move |action| {
            let w = Arc::clone(&win_clone);
            tokio::spawn(async move {
                w.lock().await.apply(action);
            });
        })
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(45)).await;

    // Option+Right -> accept 1st word (" нашей")
    for action in session.accept_next_word().await {
        window.lock().await.apply(action);
    }
    assert_eq!(
        window.lock().await.buffer,
        "Привет, хотел уточнить по поводу нашей"
    );
    assert!(window.lock().await.ghost_text.is_some());

    // Option+Right -> accept 2nd word (" встречи,")
    for action in session.accept_next_word().await {
        window.lock().await.apply(action);
    }
    assert_eq!(
        window.lock().await.buffer,
        "Привет, хотел уточнить по поводу нашей встречи,"
    );

    // Esc -> dismiss remaining ghost text without modifying buffer
    let dismiss = session.dismiss().await;
    window.lock().await.apply(dismiss);
    assert!(window.lock().await.ghost_text.is_none());
    assert_eq!(
        window.lock().await.buffer,
        "Привет, хотел уточнить по поводу нашей встречи,"
    );
}

#[tokio::test]
async fn privacy_blocks_passwords_terminals_and_secrets() {
    let session = make_mock_session();

    let blocked_cases = [
        ("1Password", false, "Master password hint"),
        ("Bitwarden", false, "My secret note"),
        ("Terminal", false, "git commit -m 'update'"),
        ("iTerm2", false, "ssh root@server"),
        ("Google Chrome", true, "my_secret_password"), // AXSecureTextField
        ("Slack", false, "Вот мой ключ sk-1234567890abcdef123456"),
        ("Mail", false, "Карта 4276 1234 5678 9012"),
    ];

    for (app_name, is_secure, text) in blocked_cases {
        let mut w = SimulatedAppWindow::new(app_name, text);
        w.is_secure_field = is_secure;
        let window = Arc::new(Mutex::new(w));
        let snap = window.lock().await.snapshot();

        let win_clone = Arc::clone(&window);
        session
            .on_text_changed(snap.clone(), move |action| {
                let w = Arc::clone(&win_clone);
                tokio::spawn(async move {
                    w.lock().await.apply(action);
                });
            })
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(35)).await;
        assert!(
            window.lock().await.ghost_text.is_none(),
            "Ghost text MUST be blocked for {} (secure={})",
            app_name,
            is_secure
        );

        let fix_res = session.fix_selection(snap).await;
        assert!(
            matches!(fix_res, SessionAction::NotifyError(_)),
            "FixGrammar MUST be blocked for {} (secure={})",
            app_name,
            is_secure
        );
    }
}

#[tokio::test]
async fn fix_layout_and_rewrite_in_notes_and_mail() {
    let session = make_mock_session();

    // Wrong layout in Notes: "ghbdtn rfr ltkf" -> "Привет, как дела?"
    let mut notes = SimulatedAppWindow::new("Notes", "ghbdtn rfr ltkf");
    let fix_action = session.fix_selection(notes.snapshot()).await;
    assert!(notes.apply(fix_action));
    assert_eq!(notes.buffer, "Привет, как дела?");

    // Selected text rewrite in Mail
    let mut mail = SimulatedAppWindow::new(
        "Mail",
        "Добрый день! Я хотел бы очень подробно обсудить детали нашего контракта.",
    );
    mail.selected_text = Some("Я хотел бы очень подробно обсудить детали нашего контракта.".into());
    let rewrite_action = session
        .rewrite_selection(mail.snapshot(), RewriteStyle::Concise)
        .await;
    assert!(mail.apply(rewrite_action));
    assert!(mail.buffer.contains("[concise]"));
}

#[tokio::test]
async fn aborts_insert_if_buffer_changed_before_apply() {
    let session = make_mock_session();
    let window = Arc::new(Mutex::new(SimulatedAppWindow::new(
        "TextEdit",
        "Привет, хотел уточнить по поводу",
    )));
    let snap = window.lock().await.snapshot();

    let win_clone = Arc::clone(&window);
    session
        .on_text_changed(snap, move |action| {
            let w = Arc::clone(&win_clone);
            tokio::spawn(async move {
                w.lock().await.apply(action);
            });
        })
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(45)).await;
    let insert = session.accept_all().await.expect("suggestion");

    // Simulate user typing extra char right before InsertText reaches the app
    window.lock().await.buffer.push('!');
    let applied = window.lock().await.apply(insert);
    assert!(!applied, "Insert must abort if prefix changed");
    assert_eq!(
        window.lock().await.buffer,
        "Привет, хотел уточнить по поводу!"
    );
}

#[test]
fn e2e_daemon_stdio_json_lines_without_server() {
    let bin = env!("CARGO_BIN_EXE_ai-assistant-for-writing");
    let mut child = Command::new(bin)
        .args(["--mock", "daemon", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon --stdio");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut lines = stdout.lines();

    // 1. Send text_changed from Chrome
    writeln!(
        stdin,
        r#"{{"event":"text_changed","app_name":"Google Chrome","is_secure_field":false,"text_before_cursor":"Hello, I wanted to follow up on","cursor_rect":{{"x":120.0,"y":300.0,"height":16.0}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    // Expect hide_ghost (debounced clear), then show_thinking, then show_ghost
    let line1 = lines.next().unwrap().unwrap();
    assert!(line1.contains(r#""cmd":"hide_ghost""#), "got: {}", line1);

    let line2 = lines.next().unwrap().unwrap();
    assert!(line2.contains(r#""cmd":"show_thinking""#), "got: {}", line2);

    let line3 = lines.next().unwrap().unwrap();
    assert!(line3.contains(r#""cmd":"show_ghost""#), "got: {}", line3);
    assert!(
        line3.contains("our meeting scheduled for tomorrow afternoon."),
        "got: {}",
        line3
    );

    // 2. Send accept_all (Tab)
    writeln!(stdin, r#"{{"event":"accept_all"}}"#).unwrap();
    stdin.flush().unwrap();
    let line3 = lines.next().unwrap().unwrap();
    assert!(line3.contains(r#""cmd":"insert_text""#), "got: {}", line3);

    // 3. Send fix_grammar with wrong layout
    writeln!(
        stdin,
        r#"{{"event":"fix_grammar","app_name":"Notes","is_secure_field":false,"text_before_cursor":"ghbdtn rfr ltkf"}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let line4 = lines.next().unwrap().unwrap();
    assert!(line4.contains(r#""cmd":"replace_selection""#), "got: {}", line4);
    assert!(line4.contains("Привет, как дела?"), "got: {}", line4);

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success());
    let _ = MockProvider;
}
