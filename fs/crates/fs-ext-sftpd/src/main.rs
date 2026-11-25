//! sftpd as an executable `fsd` extension.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use fs_common::extension::Extension;
use fs_ext_sftpd::extension::SftpdExtension;

#[derive(Parser)]
#[command(
    name = "fs-ext-sftpd",
    version,
    about = "SFTP server extension for fsd"
)]
struct Cli {
    #[arg(long)]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the daemon over stdin/stdout
    Run,
    /// Check the config, printing one problem per line to stdout
    Validate,
}

#[tokio::main]
async fn main() {
    // No timestamps: the daemon stamps each forwarded line itself.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .init();

    let cli = Cli::parse();
    let code = match cli.command {
        Command::Run => run(&cli.config).await,
        Command::Validate => validate(&cli.config),
    };
    std::process::exit(code);
}

async fn run(config: &Path) -> i32 {
    let ext = match SftpdExtension::load(config) {
        Ok(ext) => ext,
        Err(e) => {
            tracing::error!("{e:#}");
            return 1;
        }
    };
    match fs_common::extension_stdio::serve(Arc::new(ext)).await {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("{e:#}");
            1
        }
    }
}

fn validate(config: &Path) -> i32 {
    let errors = match SftpdExtension::load(config) {
        Ok(ext) => ext.validate(),
        Err(e) => vec![format!("{e:#}")],
    };
    for error in &errors {
        println!("{error}");
    }
    i32::from(!errors.is_empty())
}
