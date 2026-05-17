use filesearch::api::ipc_client::IpcClient;
use filesearch::api::ipc_protocol::{IpcRequest, IpcResponse};
use filesearch::config;
use filesearch::daemon;
use filesearch::zidx::{ZidxBuilder, ZidxIndex};

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
    Daemon {
        /// Run as a Windows Service (called by the SCM, not directly)
        #[arg(long)]
        service: bool,
    },
    /// Validate the configuration file
    ValidateConfig,
    /// Search the index (requires daemon to be running)
    Search {
        /// Name pattern or query expression; omit to match all (combine with filter flags)
        #[arg(default_value = "")]
        query: String,
        #[arg(short, long, default_value = "100")]
        limit: u32,
        #[arg(long, default_value = "0")]
        offset: u32,
        #[arg(long)]
        json: bool,
        /// Filter by extension(s). Repeat or comma-separate: --ext rs,toml
        #[arg(short = 'e', long = "ext", value_delimiter = ',')]
        ext: Vec<String>,
        /// Filter by path prefix (e.g. --path C:/projects)
        #[arg(long)]
        path: Option<String>,
        /// Filter by parent directory name or path
        #[arg(long)]
        parent: Option<String>,
        /// Filter by file size (e.g. >1mb, <=500kb, large)
        #[arg(long)]
        size: Option<String>,
        /// Filter by date modified (e.g. today, last7days, 2024-01-01)
        #[arg(long)]
        modified: Option<String>,
        /// Filter by date created (e.g. thismonth, 2024-01-01)
        #[arg(long)]
        created: Option<String>,
        /// Full-text content search
        #[arg(long)]
        content: Option<String>,
        /// Fuzzy filename match (nucleo scorer)
        #[arg(long)]
        fuzzy: Option<String>,
        /// Filter by regex pattern applied to the full path
        #[arg(long)]
        regex: Option<String>,
        /// Return only files (exclude directories)
        #[arg(long)]
        files_only: bool,
        /// Return only directories
        #[arg(long)]
        dirs_only: bool,
        /// Return only inner archive entries
        #[arg(long)]
        archive_only: bool,
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
    /// Install filesearch as a Windows Service (Windows only)
    InstallService,
    /// Uninstall the filesearch Windows Service (Windows only)
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
        Command::Daemon { service } => {
            #[cfg(target_os = "windows")]
            if service {
                if let Err(e) = filesearch::service::windows::run_as_service(cli.config) {
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
                    Err(e) => { eprintln!("cannot determine exe path: {e}"); std::process::exit(1); }
                };
                let config_path = match cli.config.canonicalize() {
                    Ok(p) => p,
                    Err(e) => { eprintln!("cannot resolve config path: {e}"); std::process::exit(1); }
                };
                if let Err(e) = filesearch::service::windows::install(&exe_path, &config_path) {
                    eprintln!("install-service error: {e}");
                    std::process::exit(1);
                }
                println!("Service '{}' installed.", filesearch::service::windows::SERVICE_NAME);
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
                if let Err(e) = filesearch::service::windows::uninstall() {
                    eprintln!("uninstall-service error: {e}");
                    std::process::exit(1);
                }
                println!("Service '{}' uninstalled.", filesearch::service::windows::SERVICE_NAME);
            }
            #[cfg(not(target_os = "windows"))]
            {
                eprintln!("uninstall-service is only supported on Windows");
                std::process::exit(1);
            }
        }
        Command::Search {
            query, limit, offset, json,
            ext, path, parent, size, modified, created,
            content, fuzzy, regex, files_only, dirs_only, archive_only,
        } => {
            let opts = SearchOpts { ext, path, parent, size, modified, created,
                                    content, fuzzy, regex, files_only, dirs_only, archive_only };
            if let Err(e) = run_search(cli.config, query, limit, offset, json, opts).await {
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

struct SearchOpts {
    ext:         Vec<String>,
    path:        Option<String>,
    parent:      Option<String>,
    size:        Option<String>,
    modified:    Option<String>,
    created:     Option<String>,
    content:     Option<String>,
    fuzzy:       Option<String>,
    regex:       Option<String>,
    files_only:  bool,
    dirs_only:   bool,
    archive_only: bool,
}

async fn run_search(
    config_path: std::path::PathBuf,
    query:       String,
    limit:       u32,
    offset:      u32,
    json:        bool,
    opts:        SearchOpts,
) -> anyhow::Result<()> {
    let mut clauses: Vec<String> = Vec::new();

    if !opts.ext.is_empty() {
        clauses.push(format!("ext:{}", opts.ext.join(";")));
    }
    if let Some(p) = opts.path     { clauses.push(format!("path:{p}")); }
    if let Some(p) = opts.parent   { clauses.push(format!("parent:{p}")); }
    if let Some(s) = opts.size     { clauses.push(format!("size:{s}")); }
    if let Some(d) = opts.modified { clauses.push(format!("dm:{d}")); }
    if let Some(d) = opts.created  { clauses.push(format!("dc:{d}")); }
    if let Some(c) = opts.content  { clauses.push(format!("content:{c}")); }
    if let Some(f) = opts.fuzzy    { clauses.push(format!("fuzzy:{f}")); }
    if let Some(r) = opts.regex    { clauses.push(format!("regex:{r}")); }
    // Modifier flags: inject a dummy `*` term so the modifier emits its SQL condition
    if opts.files_only   { clauses.push("file: *".into()); }
    if opts.dirs_only    { clauses.push("folder: *".into()); }
    if opts.archive_only { clauses.push("archive: *".into()); }

    let query = match (query.is_empty(), clauses.is_empty()) {
        (true, true)   => anyhow::bail!("specify a query or at least one filter flag"),
        (true, false)  => clauses.join(" "),
        (false, true)  => query,
        (false, false) => format!("{query} {}", clauses.join(" ")),
    };

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

// ---------------------------------------------------------------------------
// zidx helpers
// ---------------------------------------------------------------------------

fn run_zidx_build(
    zip_path: std::path::PathBuf,
    out_path: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let out_path = out_path.unwrap_or_else(|| zip_path.with_extension("zidx"));
    let count = ZidxBuilder::build(&zip_path, &out_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!(
        "wrote {} ({} nodes)",
        out_path.display(),
        count,
    );
    Ok(())
}

fn run_zidx_ls(
    zidx_path: std::path::PathBuf,
    path:      String,
    max_depth: u32,
) -> anyhow::Result<()> {
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
    idx:       &ZidxIndex,
    node_id:   u32,
    indent:    u32,
    max_depth: u32,
) -> anyhow::Result<()> {
    let node   = idx.node(node_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    let name   = idx.name(&node).map_err(|e| anyhow::anyhow!("{e}"))?;
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

fn run_zidx_stat(
    zidx_path: std::path::PathBuf,
    path:      String,
) -> anyhow::Result<()> {
    let idx     = ZidxIndex::open(&zidx_path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let node_id = idx.lookup(&path)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("path not found: {path}"))?;
    let node    = idx.node(node_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    let name    = idx.name(&node).map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("name:                 {name}");
    println!("kind:                 {}", if node.kind == 1 { "directory" } else { "file" });
    println!("node_id:              {node_id}");
    println!("parent_id:            {}", node.parent);
    println!("uncompressed_size:    {}", zidx_fmt_size(node.uncompressed_size));
    println!("compressed_size:      {}", zidx_fmt_size(node.compressed_size));
    if node.uncompressed_size > 0 {
        let ratio = node.compressed_size as f64 / node.uncompressed_size as f64 * 100.0;
        println!("compression_ratio:    {ratio:.1}%");
    }
    println!("compression_method:   {}", zidx_compression_name(node.compression));
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
        b            => format!("{b} B"),
    }
}

fn zidx_compression_name(method: u16) -> &'static str {
    match method {
        0 => "stored",
        8 => "deflated",
        _ => "unknown",
    }
}
