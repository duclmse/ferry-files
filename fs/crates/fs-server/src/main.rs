use fs_server::config;
use fs_server::daemon;
use fs_server::extensions;
use fs_server::service;
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
        /// Started by the system's service manager for every user (set by
        /// install-service --system; on Windows the SCM starts it this way)
        #[arg(long)]
        service: bool,
    },

    /// Validate the configuration file
    ValidateConfig,

    /// Generate a token for the REST file-change routes
    GenerateToken,

    /// Run fsd as a service: Windows service, systemd unit or launchd job.
    /// Starts it now and at every boot (--system) or sign-in (--user)
    InstallService {
        #[command(flatten)]
        scope: ScopeArgs,
    },

    /// Stop the service and remove it
    UninstallService {
        #[command(flatten)]
        scope: ScopeArgs,
    },

    /// Create or inspect .zidx index files
    Zidx {
        #[command(subcommand)]
        action: ZidxAction,
    },
}

#[derive(clap::Args)]
struct ScopeArgs {
    /// For your user only, while you are signed in (default on Linux and macOS)
    #[arg(long, conflicts_with = "system")]
    user: bool,
    /// For the whole computer, from boot; needs root or an administrator
    /// (default and only choice on Windows)
    #[arg(long)]
    system: bool,
}

impl ScopeArgs {
    fn scope(&self) -> service::Scope {
        if self.system {
            service::Scope::System
        } else if self.user {
            service::Scope::User
        } else {
            service::Scope::default_for_os()
        }
    }
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
            if service {
                // A service starts in `/` or System32; relative paths in the
                // config mean the config's own folder.
                if let Some(dir) = cli
                    .config
                    .canonicalize()
                    .ok()
                    .and_then(|c| c.parent().map(|d| d.to_path_buf()))
                {
                    let _ = std::env::set_current_dir(dir);
                }
            }
            #[cfg(target_os = "windows")]
            if service {
                if let Err(e) = service::windows::run_as_service(cli.config) {
                    eprintln!("service dispatcher error: {e}");
                    std::process::exit(1);
                }
                return;
            }
            let opts = daemon::RunOptions {
                system_service: service,
            };
            if let Err(e) = daemon::run(cli.config, opts).await {
                eprintln!("daemon error: {e}");
                std::process::exit(1);
            }
        }
        Command::InstallService { scope } => {
            if let Err(e) = install_service(&cli.config, scope.scope()) {
                eprintln!("install-service: {e}");
                std::process::exit(1);
            }
        }
        Command::UninstallService { scope } => match service::uninstall(scope.scope()) {
            Ok(()) => println!("Service '{}' stopped and removed.", service::SERVICE_NAME),
            Err(e) => {
                eprintln!("uninstall-service: {e}");
                std::process::exit(1);
            }
        },
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

/// Checks the config before handing it to a service manager, which would
/// otherwise only show a service that keeps failing to start.
fn install_service(config_path: &std::path::Path, scope: service::Scope) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let config_path = config_path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot find the config {}: {e}", config_path.display()))?;
    #[cfg(windows)]
    let config_path = config::without_verbatim(config_path);
    let cfg = config::load(&config_path)?;
    let errors = config::validate(&cfg);
    if !errors.is_empty() {
        anyhow::bail!("the config has errors:\n  {}", errors.join("\n  "));
    }
    let installed = service::install(&exe, &config_path, scope)?;
    let how = match scope {
        service::Scope::User => "for this user",
        service::Scope::System => "for all users",
    };
    println!(
        "Service '{}' installed {how} and started.",
        service::SERVICE_NAME
    );
    if let Some(file) = installed.file {
        println!("  {}", file.display());
    }
    for note in installed.notes {
        println!("  {note}");
    }
    Ok(())
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
