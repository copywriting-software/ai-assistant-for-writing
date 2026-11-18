use ai_assistant_for_writing::ai::create_provider;
use ai_assistant_for_writing::config::{AppConfig, ProviderKind};
use ai_assistant_for_writing::encoding::{
    convert_en_to_ru_layout, convert_ru_to_en_layout, looks_like_russian_in_en_layout,
};
use ai_assistant_for_writing::engine::{RewriteStyle, WritingEngine};
use ai_assistant_for_writing::memory::MemoryStore;
use ai_assistant_for_writing::platform::run_daemon;
use ai_assistant_for_writing::session::SessionManager;
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "ai-assistant-for-writing",
    version,
    about = "Fast async AI writing assistant (autocomplete, grammar/layout fix, rewrite)"
)]
struct Cli {
    /// Optional path to a TOML config file
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Use offline mock AI provider (no server or API key required)
    #[arg(long, global = true)]
    mock: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Complete a text prefix (inline autocomplete preview)
    Complete {
        /// Text before the cursor
        text: String,
    },
    /// Fix grammar, spelling, punctuation, and broken keyboard layout/encoding
    Fix {
        /// Text to fix
        text: String,
    },
    /// Rewrite text in a target style (concise, professional, friendly, clear)
    Rewrite {
        /// Text to rewrite
        text: String,
        #[arg(long, default_value = "concise")]
        style: String,
    },
    /// Convert text typed on the wrong keyboard layout (EN <-> RU) locally
    Layout {
        /// Text to convert
        text: String,
    },
    /// Check configuration, AI provider connectivity, and native helper availability
    Check,
    /// Show accumulated writing statistics, latency, and learned phrases
    Stats,
    /// Run background daemon (connects to macOS native helper or stdio JSON-RPC)
    Daemon {
        /// Force stdio JSON-RPC mode instead of launching the native OS helper
        #[arg(long)]
        stdio: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let mut cfg = AppConfig::load(cli.config.as_deref())?;
    if cli.mock {
        cfg.provider.kind = ProviderKind::Mock;
    }

    match cli.command {
        Commands::Layout { text } => {
            let has_cyrillic = text.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c));
            if has_cyrillic && !looks_like_russian_in_en_layout(&text) {
                println!("{}", convert_ru_to_en_layout(&text));
            } else {
                println!("{}", convert_en_to_ru_layout(&text));
            }
        }
        Commands::Stats => {
            let memory = MemoryStore::load_default();
            println!("{}", memory.summary_report());
        }
        Commands::Check => {
            println!("Provider: {:?}", cfg.provider.kind);
            println!("Base URL: {}", cfg.provider.effective_base_url());
            println!(
                "Autocomplete model: {}",
                cfg.provider.effective_autocomplete_model()
            );
            println!("Edit model: {}", cfg.provider.effective_edit_model());
            let provider = create_provider(&cfg.provider)?;
            let status = provider.health_check().await?;
            println!("Health: {}", status);
            if let Some(helper) = ai_assistant_for_writing::platform::find_macos_helper() {
                println!("macOS helper: {}", helper.display());
            } else {
                println!("macOS helper: not built yet (run `macos/build.sh`)");
            }
        }
        Commands::Complete { text } => {
            let provider = create_provider(&cfg.provider)?;
            let engine = WritingEngine::new(cfg, provider);
            let cancel = CancellationToken::new();
            match engine.complete(&text, None, false, cancel).await? {
                Some(suffix) => println!("{}{}", text, suffix),
                None => println!("{}", text),
            }
        }
        Commands::Fix { text } => {
            let provider = create_provider(&cfg.provider)?;
            let engine = WritingEngine::new(cfg, provider);
            let cancel = CancellationToken::new();
            let fixed = engine.fix_grammar(&text, None, false, cancel).await?;
            println!("{}", fixed);
        }
        Commands::Rewrite { text, style } => {
            let provider = create_provider(&cfg.provider)?;
            let engine = WritingEngine::new(cfg, provider);
            let cancel = CancellationToken::new();
            let out = engine
                .rewrite(&text, RewriteStyle::from_str(&style), None, false, cancel)
                .await?;
            println!("{}", out);
        }
        Commands::Daemon { stdio } => {
            let provider = create_provider(&cfg.provider)?;
            let memory = Arc::new(MemoryStore::load_default());
            let engine = Arc::new(WritingEngine::with_memory(cfg, provider, memory));
            let session = Arc::new(SessionManager::new(engine));
            run_daemon(session, stdio).await?;
        }
    }

    Ok(())
}
