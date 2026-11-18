use crate::engine::RewriteStyle;
use crate::session::{CursorRect, FocusSnapshot, SessionAction, SessionManager};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum NativeEvent {
    TextChanged(FocusSnapshot),
    AcceptAll,
    AcceptWord,
    Dismiss,
    FixGrammar(FocusSnapshot),
    Rewrite {
        #[serde(flatten)]
        snapshot: FocusSnapshot,
        #[serde(default = "default_style")]
        style: String,
    },
}

fn default_style() -> String {
    "concise".to_string()
}

#[derive(Debug, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum NativeCommand {
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
    NotifyError {
        message: String,
    },
}

impl From<SessionAction> for NativeCommand {
    fn from(action: SessionAction) -> Self {
        match action {
            SessionAction::ShowThinking { rect } => Self::ShowThinking { rect },
            SessionAction::ShowGhost { text, rect } => Self::ShowGhost { text, rect },
            SessionAction::HideGhost => Self::HideGhost,
            SessionAction::InsertText {
                text,
                expected_prefix,
            } => Self::InsertText {
                text,
                expected_prefix,
            },
            SessionAction::ReplaceSelection { text } => Self::ReplaceSelection { text },
            SessionAction::NotifyError(message) => Self::NotifyError { message },
        }
    }
}

pub fn find_macos_helper() -> Option<PathBuf> {
    let candidates = [
        PathBuf::from("macos/AIWritingHelper.app/Contents/MacOS/ai-writing-macos-helper"),
        PathBuf::from("macos/ai-writing-macos-helper"),
        PathBuf::from("./ai-writing-macos-helper"),
    ];
    for c in candidates {
        if c.exists() {
            return Some(c);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("ai-writing-macos-helper");
            if sibling.exists() {
                return Some(sibling);
            }
        }
    }
    None
}

/// Runs the daemon bridging events between the native OS helper (or stdin/stdout in headless mode)
/// and the async Rust `SessionManager`.
pub async fn run_daemon(session: Arc<SessionManager>, stdio_only: bool) -> Result<()> {
    if cfg!(target_os = "macos") && !stdio_only {
        if let Some(helper_path) = find_macos_helper() {
            tracing::info!("Starting macOS native helper: {}", helper_path.display());
            return run_with_helper(session, helper_path).await;
        }
        tracing::warn!(
            "macOS native helper binary not found (run `macos/build.sh` to compile it). Falling back to stdio JSON-RPC mode."
        );
    } else if !stdio_only {
        tracing::info!(
            "Native OS adapter for this platform is planned for v0.5/v0.6; running in stdio JSON-RPC mode."
        );
    }

    run_stdio_loop(session).await
}

async fn run_with_helper(session: Arc<SessionManager>, helper_path: PathBuf) -> Result<()> {
    let mut child = Command::new(&helper_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("Failed to spawn macOS helper at {}", helper_path.display()))?;

    let stdout = child.stdout.take().context("Missing helper stdout")?;
    let stdin = child.stdin.take().context("Missing helper stdin")?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<NativeCommand>();

    let writer_handle = tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(cmd) = rx.recv().await {
            if let Ok(line) = serde_json::to_string(&cmd) {
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if stdin.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
        }
    });

    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        handle_event_line(&session, &line, &tx).await;
    }

    drop(tx);
    let _ = writer_handle.await;
    let status = child.wait().await?;
    tracing::info!("macOS helper exited with status: {}", status);
    Ok(())
}

async fn run_stdio_loop(session: Arc<SessionManager>) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<NativeCommand>();
    let writer_handle = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(cmd) = rx.recv().await {
            if let Ok(line) = serde_json::to_string(&cmd) {
                let _ = stdout.write_all(line.as_bytes()).await;
                let _ = stdout.write_all(b"\n").await;
                let _ = stdout.flush().await;
            }
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        handle_event_line(&session, &line, &tx).await;
    }

    drop(tx);
    let _ = writer_handle.await;
    Ok(())
}

async fn handle_event_line(
    session: &Arc<SessionManager>,
    line: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<NativeCommand>,
) {
    let event: NativeEvent = match serde_json::from_str(line) {
        Ok(ev) => ev,
        Err(e) => {
            tracing::warn!("Invalid native event JSON: {} ({})", e, line);
            return;
        }
    };

    match event {
        NativeEvent::TextChanged(snapshot) => {
            let tx_c = tx.clone();
            session
                .on_text_changed(snapshot, move |action| {
                    let _ = tx_c.send(action.into());
                })
                .await;
        }
        NativeEvent::AcceptAll => {
            if let Some(action) = session.accept_all().await {
                let _ = tx.send(action.into());
            }
        }
        NativeEvent::AcceptWord => {
            for action in session.accept_next_word().await {
                let _ = tx.send(action.into());
            }
        }
        NativeEvent::Dismiss => {
            let action = session.dismiss().await;
            let _ = tx.send(action.into());
        }
        NativeEvent::FixGrammar(snapshot) => {
            let session_c = Arc::clone(session);
            let tx_c = tx.clone();
            tokio::spawn(async move {
                let action = session_c.fix_selection(snapshot).await;
                let _ = tx_c.send(action.into());
            });
        }
        NativeEvent::Rewrite { snapshot, style } => {
            let session_c = Arc::clone(session);
            let tx_c = tx.clone();
            tokio::spawn(async move {
                let action = session_c
                    .rewrite_selection(snapshot, RewriteStyle::from_str(&style))
                    .await;
                let _ = tx_c.send(action.into());
            });
        }
    }
}
