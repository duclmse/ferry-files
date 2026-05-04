use clap::{Parser, Subcommand};
use filesearch::config;

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

fn main() {
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
        Command::Daemon
        | Command::Search { .. }
        | Command::Stats
        | Command::Reindex { .. }
        | Command::Open { .. }
        | Command::Tui => {
            eprintln!("Not yet implemented — see Phase 3–6.");
            std::process::exit(1);
        }
    }
}
