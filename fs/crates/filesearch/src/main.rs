use filesearch::api::ipc_client::IpcClient;
use filesearch::api::ipc_protocol::{IpcRequest, IpcResponse};
use filesearch::config;
use filesearch::daemon;

use clap::{Parser, Subcommand};

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
        limit: u32,
        #[arg(long, default_value = "0")]
        offset: u32,
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
    /// Open a file via the daemon
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
            if let Err(e) = daemon::run(cli.config).await {
                eprintln!("daemon error: {e}");
                std::process::exit(1);
            }
        }
        Command::Search { query, limit, offset, json } => {
            if let Err(e) = run_search(cli.config, query, limit, offset, json).await {
                eprintln!("search error: {e}");
                std::process::exit(1);
            }
        }
        Command::Stats => {
            if let Err(e) = run_stats(cli.config).await {
                eprintln!("stats error: {e}");
                std::process::exit(1);
            }
        }
        Command::Reindex { full } => {
            if let Err(e) = run_reindex(cli.config, full).await {
                eprintln!("reindex error: {e}");
                std::process::exit(1);
            }
        }
        Command::Open { path } => {
            if let Err(e) = run_open(cli.config, path).await {
                eprintln!("open error: {e}");
                std::process::exit(1);
            }
        }
        Command::Tui => {
            if let Err(e) = run_tui(cli.config).await {
                eprintln!("tui error: {e}");
                std::process::exit(1);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// IPC client helpers
// ---------------------------------------------------------------------------

async fn ipc_client_for(
    config_path: std::path::PathBuf,
) -> anyhow::Result<IpcClient> {
    let cfg = config::load(&config_path)
        .map_err(|e| anyhow::anyhow!("config load error: {e}"))?;
    IpcClient::connect(&cfg.api.ipc_path)
        .await
        .map_err(|e| anyhow::anyhow!("ipc connect: {e}"))
}

async fn run_search(
    config_path: std::path::PathBuf,
    query:       String,
    limit:       u32,
    offset:      u32,
    json:        bool,
) -> anyhow::Result<()> {
    let mut client = ipc_client_for(config_path).await?;
    let resp = client
        .request(IpcRequest::Search { query, limit, offset })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if let IpcResponse::Search(sr) = resp {
        if json {
            println!("{}", serde_json::to_string_pretty(&sr)?);
        } else {
            for hit in &sr.hits {
                println!("{}", hit.path);
            }
            eprintln!("{} hit(s) (total: {})", sr.hits.len(), sr.total_hits);
        }
    }
    Ok(())
}

async fn run_stats(config_path: std::path::PathBuf) -> anyhow::Result<()> {
    let mut client = ipc_client_for(config_path).await?;
    let resp = client
        .request(IpcRequest::Stats)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if let IpcResponse::Stats(s) = resp {
        println!("files:           {}", s.total_files);
        println!("dirs:            {}", s.total_dirs);
        println!("content indexed: {}", s.content_indexed);
        println!("archive entries: {}", s.archive_entries_indexed);
        println!("archives:        {}", s.archives_crawled);
        println!("events:          {}", s.events_processed);
        println!("last crawl:      {}s ago", s.last_crawl_secs);
    }
    Ok(())
}

async fn run_reindex(config_path: std::path::PathBuf, full: bool) -> anyhow::Result<()> {
    let mut client = ipc_client_for(config_path).await?;
    let resp = client
        .request(IpcRequest::Reindex { full })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if let IpcResponse::Reindex { accepted } = resp {
        if accepted {
            println!("reindex accepted");
        } else {
            eprintln!("reindex rejected by daemon");
            std::process::exit(1);
        }
    }
    Ok(())
}

async fn run_open(config_path: std::path::PathBuf, path: String) -> anyhow::Result<()> {
    let mut client = ipc_client_for(config_path).await?;
    let resp = client
        .request(IpcRequest::Open { path })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if let IpcResponse::Open { opened, temp_path } = resp {
        if opened {
            if let Some(p) = temp_path {
                println!("opened: {p}");
            } else {
                println!("opened");
            }
        } else {
            eprintln!("daemon could not open the file");
            std::process::exit(1);
        }
    }
    Ok(())
}

async fn run_tui(config_path: std::path::PathBuf) -> anyhow::Result<()> {
    let cfg = config::load(&config_path)
        .map_err(|e| anyhow::anyhow!("config load error: {e}"))?;
    filesearch::frontend::tui::run(&cfg.api.ipc_path).await
}
