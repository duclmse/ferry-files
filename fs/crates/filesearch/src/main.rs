use std::sync::Arc;

use clap::{Parser, Subcommand};
use filesearch::config;
use filesearch::indexer::crawler::{full_crawl, process_event, IndexMetrics};
use filesearch::store::{sqlite_store::SqliteStore, tantivy_store::TantivyStore};
use filesearch::watcher;
use filesearch::indexer::crawler::IndexEvent;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Parser)]
#[command(name = "filesearch", version, about = "Cross-platform file-search daemon")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: std::path::PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the search daemon
    Daemon,
    /// Validate the configuration file
    ValidateConfig,
    /// Search the index (requires daemon to be running)
    Search {
        query: String,
        #[arg(short, long, default_value = "100")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show index statistics
    Stats,
    /// Trigger a re-index
    Reindex {
        #[arg(long)]
        full: bool,
    },
    /// Open a file
    Open { path: String },
    /// Launch the interactive TUI
    Tui,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::ValidateConfig => {
            match config::load(&cli.config) {
                Ok(cfg) => {
                    let errors = config::validate(&cfg);
                    if errors.is_empty() {
                        println!("Config OK");
                        std::process::exit(0);
                    } else {
                        for e in &errors {
                            eprintln!("ERROR: {e}");
                        }
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("Failed to load config: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::Daemon => {
            if let Err(e) = run_daemon(cli.config).await {
                eprintln!("daemon error: {e}");
                std::process::exit(1);
            }
        }
        Command::Search { .. }
        | Command::Stats
        | Command::Reindex { .. }
        | Command::Open { .. }
        | Command::Tui => {
            eprintln!("Not yet implemented — see Phase 4–6.");
            std::process::exit(1);
        }
    }
}

async fn run_daemon(config_path: std::path::PathBuf) -> anyhow::Result<()> {
    // Load and validate config
    let cfg = config::load(&config_path)
        .map_err(|e| anyhow::anyhow!("config load error: {e}"))?;
    let errors = config::validate(&cfg);
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("Config error: {e}");
        }
        anyhow::bail!("invalid configuration — fix errors above and restart");
    }

    // Create runtime directories
    std::fs::create_dir_all(&cfg.data_dir)?;
    std::fs::create_dir_all(&cfg.log_dir)?;

    // Initialise tracing: stderr + rolling file
    let file_appender = tracing_appender::rolling::daily(&cfg.log_dir, "filesearch.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .init();

    tracing::info!("filesearch daemon starting");

    // Initialise stores
    let sqlite = Arc::new(
        SqliteStore::new(&cfg.data_dir.join("index.db"))
            .map_err(|e| anyhow::anyhow!("sqlite init: {e}"))?,
    );
    let tantivy = Arc::new(
        TantivyStore::new(&cfg.data_dir.join("tantivy"))
            .map_err(|e| anyhow::anyhow!("tantivy init: {e}"))?,
    );
    let metrics = IndexMetrics::new();

    // Shared config + cancellation
    let shared_cfg = Arc::new(RwLock::new(cfg));
    let shutdown   = CancellationToken::new();

    // Ctrl+C → cancel the token
    {
        let shutdown_c = shutdown.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("ctrl-c received, shutting down");
                shutdown_c.cancel();
            }
        });
    }

    // Config hot-reload watcher
    {
        let shutdown_c  = shutdown.clone();
        let shared_cfg2 = Arc::clone(&shared_cfg);
        tokio::spawn(config::watch_config(config_path, shared_cfg2, shutdown_c));
    }

    // Filesystem watcher → IndexEvent channel
    let (ev_tx, mut ev_rx) = mpsc::channel::<IndexEvent>(1024);
    {
        let shutdown_c = shutdown.clone();
        let cfg_c      = Arc::clone(&shared_cfg);
        tokio::spawn(async move {
            if let Err(e) = watcher::run_watcher(cfg_c, ev_tx, shutdown_c).await {
                tracing::error!("watcher error: {e}");
            }
        });
    }

    // Initial full crawl (runs in background; watcher events processed in parallel)
    {
        let sqlite_c   = Arc::clone(&sqlite);
        let tantivy_c  = Arc::clone(&tantivy);
        let metrics_c  = Arc::clone(&metrics);
        let cfg_c      = Arc::clone(&shared_cfg);
        let shutdown_c = shutdown.clone();
        tokio::spawn(async move {
            tracing::info!("starting full crawl");
            if let Err(e) = full_crawl(cfg_c, sqlite_c, tantivy_c, metrics_c, shutdown_c).await {
                tracing::error!("full_crawl error: {e}");
            } else {
                tracing::info!("full crawl complete");
            }
        });
    }

    // Main event loop: dispatch watcher events to process_event
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("filesearch daemon shut down");
                break;
            }
            Some(event) = ev_rx.recv() => {
                let sqlite_e  = Arc::clone(&sqlite);
                let tantivy_e = Arc::clone(&tantivy);
                let metrics_e = Arc::clone(&metrics);
                let cfg_e     = Arc::clone(&shared_cfg);
                tokio::spawn(async move {
                    process_event(event, cfg_e, sqlite_e, tantivy_e, metrics_e).await;
                });
            }
        }
    }

    Ok(())
}
