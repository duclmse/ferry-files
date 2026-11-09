use fs_server::config;
use fs_server::daemon;
use fs_server::extensions;
use fs_server::zidx::{ZidxBuilder, ZidxIndex};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fsd", version, about = "Cross-platform file-search daemon")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: std::path::PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the search daemon
    Daemon {
        /// Run as a Windows Service (called by the SCM, not directly)
        #[arg(long)]
        service: bool,
    },

    /// Validate the configuration file
    ValidateConfig,

    /// Generate a token for the REST file-change routes
    GenerateToken,

    /// Install fsd as a Windows Service (Windows only)
    InstallService,

    /// Uninstall the fsd Windows Service (Windows only)
    UninstallService,

    /// Create or inspect .zidx index files
    Zidx {
        #[command(subcommand)]
        action: ZidxAction,
    },
}

#[derive(Subcommand)]
enum ZidxAction {
    /// Build a .zidx sidecar index from a ZIP file
    Build {
        /// Source ZIP file
        zip: std::path::PathBuf,
        /// Output path (default: <zip-stem>.zidx next to the ZIP)
        #[arg(short, long)]
        out: Option<std::path::PathBuf>,
    },

    /// List entries in a .zidx index file as an indented tree
    Ls {
        /// Path to the .zidx file
        zidx: std::path::PathBuf,
        /// Sub-path inside the archive to list from (default: root)
        #[arg(default_value = "")]
        path: String,
        /// Maximum recursion depth (0 = unlimited)
        #[arg(short, long, default_value = "0")]
        depth: u32,
    },

    /// Show metadata for a specific entry in a .zidx index
    Stat {
        /// Path to the .zidx file
        zidx: std::path::PathBuf,
        /// Path of the entry inside the archive (e.g. src/main.rs)
        path: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::GenerateToken => {
            let (token, hash) = config::generate_file_token();
            println!("Token (shown once, send as `Authorization: Bearer <token>`):");
            println!("  {token}");
            println!("Add under [api] in the config:");
            println!("  auth_token_hash = \"{hash}\"");
        }
        Command::ValidateConfig => match config::load(&cli.config) {
            Ok(cfg) => {
                let mut errors = config::validate(&cfg);
                match extensions::load(&cli.config) {
                    Ok(loaded) => {
                        errors.extend(loaded.problems);
                        for ext in &loaded.extensions {
                            let name = ext.name().to_owned();
                            errors.extend(
                                ext.validate()
                                    .into_iter()
                                    .map(|e| format!("extension {name}: {e}")),
                            );
                        }
                    }
                    Err(e) => errors.push(e.to_string()),
                }
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
        },
        Command::Daemon { service } => {
            #[cfg(target_os = "windows")]
            if service {
                if let Err(e) = fs_server::service::windows::run_as_service(cli.config) {
                    eprintln!("service dispatcher error: {e}");
                    std::process::exit(1);
                }
                return;
            }
            #[cfg(not(target_os = "windows"))]
            if service {
                eprintln!("--service is only supported on Windows");
                std::process::exit(1);
            }
            if let Err(e) = daemon::run(cli.config).await {
                eprintln!("daemon error: {e}");
                std::process::exit(1);
            }
        }
        Command::InstallService => {
            #[cfg(target_os = "windows")]
            {
                let exe_path = match std::env::current_exe() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("cannot determine exe path: {e}");
                        std::process::exit(1);
                    }
                };
                let config_path = match cli.config.canonicalize() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("cannot resolve config path: {e}");
                        std::process::exit(1);
                    }
                };
                if let Err(e) = fs_server::service::windows::install(&exe_path, &config_path) {
                    eprintln!("install-service error: {e}");
                    std::process::exit(1);
                }
                println!(
                    "Service '{}' installed.",
                    fs_server::service::windows::SERVICE_NAME
                );
            }
            #[cfg(not(target_os = "windows"))]
            {
                eprintln!("install-service is only supported on Windows");
                std::process::exit(1);
            }
        }
        Command::UninstallService => {
            #[cfg(target_os = "windows")]
            {
                if let Err(e) = fs_server::service::windows::uninstall() {
                    eprintln!("uninstall-service error: {e}");
                    std::process::exit(1);
                }
                println!(
                    "Service '{}' uninstalled.",
                    fs_server::service::windows::SERVICE_NAME
                );
            }
            #[cfg(not(target_os = "windows"))]
            {
                eprintln!("uninstall-service is only supported on Windows");
                std::process::exit(1);
            }
        }
        Command::Zidx { action } => {
            let result = match action {
                ZidxAction::Build { zip, out } => run_zidx_build(zip, out),
                ZidxAction::Ls { zidx, path, depth } => run_zidx_ls(zidx, path, depth),
                ZidxAction::Stat { zidx, path } => run_zidx_stat(zidx, path),
            };
            if let Err(e) = result {
                eprintln!("zidx error: {e}");
                std::process::exit(1);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// zidx helpers
// ---------------------------------------------------------------------------

fn run_zidx_build(
    zip_path: std::path::PathBuf,
    out_path: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let out_path = out_path.unwrap_or_else(|| zip_path.with_extension("zidx"));
    let count = ZidxBuilder::build(&zip_path, &out_path).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("wrote {} ({} nodes)", out_path.display(), count,);
    Ok(())
}

fn run_zidx_ls(zidx_path: std::path::PathBuf, path: String, max_depth: u32) -> anyhow::Result<()> {
    let idx = ZidxIndex::open(&zidx_path).map_err(|e| anyhow::anyhow!("{e}"))?;

    let start_id = if path.is_empty() {
        0
    } else {
        idx.lookup(&path)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .ok_or_else(|| anyhow::anyhow!("path not found: {path}"))?
    };

    zidx_print_tree(&idx, start_id, 0, max_depth)?;
    Ok(())
}

fn zidx_print_tree(
    idx: &ZidxIndex,
    node_id: u32,
    indent: u32,
    max_depth: u32,
) -> anyhow::Result<()> {
    let node = idx.node(node_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    let name = idx.name(&node).map_err(|e| anyhow::anyhow!("{e}"))?;
    let prefix = "  ".repeat(indent as usize);

    if node.kind == 1 {
        println!("{prefix}{name}/");
        if max_depth == 0 || indent < max_depth {
            for child in idx.children(node_id) {
                let child_id = child.map_err(|e| anyhow::anyhow!("{e}"))?;
                zidx_print_tree(idx, child_id, indent + 1, max_depth)?;
            }
        }
    } else {
        println!(
            "{prefix}{name}  {} [{}]",
            zidx_fmt_size(node.uncompressed_size),
            zidx_compression_name(node.compression),
        );
    }
    Ok(())
}

fn run_zidx_stat(zidx_path: std::path::PathBuf, path: String) -> anyhow::Result<()> {
    let idx = ZidxIndex::open(&zidx_path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let node_id = idx
        .lookup(&path)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("path not found: {path}"))?;
    let node = idx.node(node_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    let name = idx.name(&node).map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("name:                 {name}");
    println!(
        "kind:                 {}",
        if node.kind == 1 { "directory" } else { "file" }
    );
    println!("node_id:              {node_id}");
    println!("parent_id:            {}", node.parent);
    println!(
        "uncompressed_size:    {}",
        zidx_fmt_size(node.uncompressed_size)
    );
    println!(
        "compressed_size:      {}",
        zidx_fmt_size(node.compressed_size)
    );
    if node.uncompressed_size > 0 {
        let ratio = node.compressed_size as f64 / node.uncompressed_size as f64 * 100.0;
        println!("compression_ratio:    {ratio:.1}%");
    }
    println!(
        "compression_method:   {}",
        zidx_compression_name(node.compression)
    );
    println!("local_header_offset:  {}", node.local_header_offset);
    Ok(())
}

fn zidx_fmt_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    match bytes {
        b if b >= GB => format!("{:.2} GB", b as f64 / GB as f64),
        b if b >= MB => format!("{:.2} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{:.2} KB", b as f64 / KB as f64),
        b => format!("{b} B"),
    }
}

fn zidx_compression_name(method: u16) -> &'static str {
    match method {
        0 => "stored",
        8 => "deflated",
        _ => "unknown",
    }
}
