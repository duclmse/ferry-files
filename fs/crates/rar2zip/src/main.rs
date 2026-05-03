use clap::{Parser, Subcommand};

mod config;
mod error;

#[derive(Parser)]
#[command(
    name = "rar2zip",
    version,
    about = "Watch folder → convert archives → SFTP transfer"
)]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: std::path::PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the service (direct mode or Windows Service)
    Run {
        #[arg(long)]
        service: bool,
    },
    /// Print queue depth table
    Status,
    /// Reset all failed jobs back to pending
    RetryFailed,
    /// Validate the configuration file (and TCP-ping each destination)
    ValidateConfig,
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
        Command::Run { .. } | Command::Status | Command::RetryFailed => {
            eprintln!("Not yet implemented — see Phase 4–6.");
            std::process::exit(1);
        }
    }
}
