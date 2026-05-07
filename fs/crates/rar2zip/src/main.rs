use clap::{Parser, Subcommand};
use rar2zip::config;

#[derive(Parser)]
#[command(
    name    = "rar2zip",
    version,
    about   = "Watch folder → convert archives → SFTP transfer"
)]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: std::path::PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the service (direct mode, or Windows Service when --service is set)
    Run {
        #[arg(long)]
        service: bool,
    },
    /// Print queue depth per status
    Status,
    /// Reset all failed jobs back to pending
    RetryFailed,
    /// Validate the config file and TCP-ping each destination
    ValidateConfig,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        // ------------------------------------------------------------------
        Command::ValidateConfig => {
            match config::load(&cli.config) {
                Ok(cfg) => {
                    let errors = config::validate(&cfg);
                    if errors.is_empty() {
                        println!("Config OK");
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

        // ------------------------------------------------------------------
        Command::Run { service } => {
            // Windows Service dispatcher (blocks until the service stops)
            #[cfg(target_os = "windows")]
            if service {
                if let Err(e) =
                    rar2zip::service::windows::run_as_service(cli.config)
                {
                    eprintln!("Service dispatcher error: {e}");
                    std::process::exit(1);
                }
                return;
            }
            #[cfg(not(target_os = "windows"))]
            if service {
                eprintln!("--service is only supported on Windows");
                std::process::exit(1);
            }

            // Direct (non-service) run
            let cfg = match config::load(&cli.config) {
                Ok(c)  => c,
                Err(e) => {
                    eprintln!("Failed to load config: {e}");
                    std::process::exit(1);
                }
            };

            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async {
                let shutdown      = tokio_util::sync::CancellationToken::new();
                let shutdown_ctrl = shutdown.clone();
                tokio::spawn(async move {
                    tokio::signal::ctrl_c().await.ok();
                    tracing::info!("Ctrl-C received, initiating shutdown");
                    shutdown_ctrl.cancel();
                });
                if let Err(e) = rar2zip::app::run(cfg, shutdown).await {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            });
        }

        // ------------------------------------------------------------------
        Command::Status => {
            let cfg = match config::load(&cli.config) {
                Ok(c)  => c,
                Err(e) => { eprintln!("Failed to load config: {e}"); std::process::exit(1); }
            };
            let db = cfg.watch_folder.join(".rar2zip.db");
            let queue = match rar2zip::queue::JobQueue::new(&db) {
                Ok(q)  => q,
                Err(e) => { eprintln!("Error opening queue: {e}"); std::process::exit(1); }
            };
            match queue.queue_depth() {
                Ok(depth) => {
                    println!("{:<14} COUNT", "STATUS");
                    println!("{}", "-".repeat(20));
                    for status in &["pending", "converting", "transferring", "done", "failed"] {
                        println!("{:<14} {}", status, depth.get(*status).copied().unwrap_or(0));
                    }
                }
                Err(e) => { eprintln!("Error: {e}"); std::process::exit(1); }
            }
        }

        // ------------------------------------------------------------------
        Command::RetryFailed => {
            let cfg = match config::load(&cli.config) {
                Ok(c)  => c,
                Err(e) => { eprintln!("Failed to load config: {e}"); std::process::exit(1); }
            };
            let db = cfg.watch_folder.join(".rar2zip.db");
            let queue = match rar2zip::queue::JobQueue::new(&db) {
                Ok(q)  => q,
                Err(e) => { eprintln!("Error opening queue: {e}"); std::process::exit(1); }
            };
            match queue.reset_failed() {
                Ok(n)  => println!("Reset {n} failed job(s) to pending."),
                Err(e) => { eprintln!("Error: {e}"); std::process::exit(1); }
            }
        }
    }
}
