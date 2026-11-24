mod config;
mod tui;

use fs_protocol::ipc_client::IpcClient;
use fs_protocol::ipc_protocol::{IpcRequest, IpcResponse, SearchHitDto, SearchResponse};
use fs_protocol::{attrib, efu};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fs", version, about = "Client for the fs search daemon")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: std::path::PathBuf,

    #[command(subcommand)]
    command: Command,
}

// Parsed once per process, so boxing the large `Search` variant buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
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
        /// Write the results to this file as an Everything file list (.efu)
        /// instead of printing them; --limit and --offset still apply
        #[arg(long, value_name = "FILE")]
        efu: Option<std::path::PathBuf>,
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

    /// Send a command to an extension (e.g. `fs ext rar2zip status`)
    Ext {
        /// Extension name
        name: String,
        /// Command to run
        command: String,
    },

    /// List, add or remove the folders the daemon indexes and watches
    Watch {
        #[command(subcommand)]
        action: Option<WatchAction>,
    },
}

#[derive(Subcommand)]
enum WatchAction {
    /// List the watch paths (the default)
    List,
    /// Add a folder; the daemon crawls it, then watches it
    Add { path: std::path::PathBuf },
    /// Remove a watch path; its entries stay in the index unless --forget
    Remove {
        path: std::path::PathBuf,
        /// Remove its entries from the index too
        #[arg(long)]
        forget: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Search {
            query,
            limit,
            offset,
            json,
            ext,
            path,
            parent,
            size,
            modified,
            created,
            content,
            fuzzy,
            regex,
            files_only,
            dirs_only,
            archive_only,
            efu,
        } => {
            let opts = SearchOpts {
                ext,
                path,
                parent,
                size,
                modified,
                created,
                content,
                fuzzy,
                regex,
                files_only,
                dirs_only,
                archive_only,
            };
            let output = match efu {
                Some(file) => Output::Efu(file),
                None if json => Output::Json,
                None => Output::Paths,
            };
            if let Err(e) = run_search(cli.config, query, limit, offset, output, opts).await {
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
        Command::Ext { name, command } => {
            if let Err(e) = run_extension(cli.config, &name, &command).await {
                eprintln!("extension error: {e}");
                std::process::exit(1);
            }
        }
        Command::Watch { action } => {
            if let Err(e) = run_watch(cli.config, action.unwrap_or(WatchAction::List)).await {
                eprintln!("watch error: {e}");
                std::process::exit(1);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// IPC client helpers
// ---------------------------------------------------------------------------

async fn ipc_client_for(config_path: std::path::PathBuf) -> anyhow::Result<IpcClient> {
    let ipc_path = config::load_ipc_path(&config_path)?;
    IpcClient::connect(&ipc_path)
        .await
        .map_err(|e| anyhow::anyhow!("ipc connect: {e}"))
}

enum Output {
    Paths,
    Json,
    Efu(std::path::PathBuf),
}

struct SearchOpts {
    ext: Vec<String>,
    path: Option<String>,
    parent: Option<String>,
    size: Option<String>,
    modified: Option<String>,
    created: Option<String>,
    content: Option<String>,
    fuzzy: Option<String>,
    regex: Option<String>,
    files_only: bool,
    dirs_only: bool,
    archive_only: bool,
}

async fn run_search(
    config_path: std::path::PathBuf,
    query: String,
    limit: u32,
    offset: u32,
    output: Output,
    opts: SearchOpts,
) -> anyhow::Result<()> {
    let mut clauses: Vec<String> = Vec::new();

    if !opts.ext.is_empty() {
        clauses.push(format!("ext:{}", opts.ext.join(";")));
    }
    if let Some(p) = opts.path {
        clauses.push(format!("path:{p}"));
    }
    if let Some(p) = opts.parent {
        clauses.push(format!("parent:{p}"));
    }
    if let Some(s) = opts.size {
        clauses.push(format!("size:{s}"));
    }
    if let Some(d) = opts.modified {
        clauses.push(format!("dm:{d}"));
    }
    if let Some(d) = opts.created {
        clauses.push(format!("dc:{d}"));
    }
    if let Some(c) = opts.content {
        clauses.push(format!("content:{c}"));
    }
    if let Some(f) = opts.fuzzy {
        clauses.push(format!("fuzzy:{f}"));
    }
    if let Some(r) = opts.regex {
        clauses.push(format!("path: regex:{r}"));
    }
    // Modifier flags: inject a dummy `*` term so the modifier emits its SQL condition
    if opts.files_only {
        clauses.push("file: *".into());
    }
    if opts.dirs_only {
        clauses.push("folder: *".into());
    }
    if opts.archive_only {
        clauses.push("archive: *".into());
    }

    let query = match (query.is_empty(), clauses.is_empty()) {
        (true, true) => anyhow::bail!("specify a query or at least one filter flag"),
        (true, false) => clauses.join(" "),
        (false, true) => query,
        (false, false) => format!("{query} {}", clauses.join(" ")),
    };

    let mut client = ipc_client_for(config_path).await?;
    if let Output::Efu(file) = output {
        let written = export_efu(&mut client, &query, limit, offset, &file).await?;
        eprintln!("wrote {written} entries to {}", file.display());
        return Ok(());
    }
    let sr = search_page(&mut client, &query, limit, offset).await?;
    if let Output::Json = output {
        println!("{}", serde_json::to_string_pretty(&sr)?);
    } else {
        for hit in &sr.hits {
            println!("{}", hit.path);
        }
        eprintln!("{} hit(s) (total: {})", sr.hits.len(), sr.total_hits);
    }
    Ok(())
}

async fn search_page(
    client: &mut IpcClient,
    query: &str,
    limit: u32,
    offset: u32,
) -> anyhow::Result<SearchResponse> {
    let resp = client
        .request(IpcRequest::Search {
            query: query.to_owned(),
            limit,
            offset,
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    match resp {
        IpcResponse::Search(sr) => Ok(sr),
        IpcResponse::Error { message, .. } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Pages through the results, since the daemon caps one response at
/// `search.max_limit`, and writes them as one list.
async fn export_efu(
    client: &mut IpcClient,
    query: &str,
    limit: u32,
    offset: u32,
    file: &std::path::Path,
) -> anyhow::Result<u32> {
    const PAGE: u32 = 1000;
    let mut text = efu::HEADER.to_owned();
    let mut written = 0;
    loop {
        let want = (limit - written).min(PAGE);
        if want == 0 {
            break;
        }
        let sr = search_page(client, query, want, offset + written).await?;
        for hit in &sr.hits {
            efu::write_entry(&mut text, &efu_entry(hit));
        }
        let got = sr.hits.len() as u32;
        written += got;
        if got < want || u64::from(offset + written) >= sr.total_hits {
            break;
        }
    }
    std::fs::write(file, text)
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", file.display()))?;
    Ok(written)
}

fn efu_entry(hit: &SearchHitDto) -> efu::Entry {
    let folder = if hit.is_dir {
        attrib::WINDOWS_DIRECTORY
    } else {
        0
    };
    efu::Entry {
        path: hit.path.clone(),
        // Folders have no size of their own in the index.
        size: (!hit.is_dir).then_some(hit.size_bytes),
        // 0 is the index's "unknown".
        mtime: (hit.mtime > 0).then_some(hit.mtime),
        ctime: (hit.ctime > 0).then_some(hit.ctime),
        attributes: Some(attrib::to_windows(hit.attrib_bitmask) | folder),
    }
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        println!("last crawl:      {}", crawl_age(s.last_crawl_secs, now));
    }
    Ok(())
}

/// `last_crawl` is the Unix time the daemon's last full crawl finished.
fn crawl_age(last_crawl: u64, now: u64) -> String {
    if last_crawl == 0 {
        return "never".into();
    }
    let s = now.saturating_sub(last_crawl);
    let (d, h, m) = (s / 86_400, s / 3_600 % 24, s / 60 % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s ago"),
        (0, 0, _) => format!("{m}m {}s ago", s % 60),
        (0, _, _) => format!("{h}h {m}m ago"),
        _ => format!("{d}d {h}h ago"),
    }
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
    let ipc_path = config::load_ipc_path(&config_path)?;
    tui::run(&ipc_path).await
}

async fn run_extension(
    config_path: std::path::PathBuf,
    name: &str,
    command: &str,
) -> anyhow::Result<()> {
    let mut client = ipc_client_for(config_path).await?;
    let resp = client
        .request(IpcRequest::Extension {
            name: name.to_owned(),
            command: command.to_owned(),
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if let IpcResponse::Extension { output } = resp {
        println!("{output}");
    }
    Ok(())
}

async fn run_watch(config_path: std::path::PathBuf, action: WatchAction) -> anyhow::Result<()> {
    // The daemon resolves paths from its own working folder, not this one.
    let absolute = |p: std::path::PathBuf| -> anyhow::Result<String> {
        let p = std::path::absolute(&p)?;
        Ok(p.to_string_lossy().into_owned())
    };
    let request = match action {
        WatchAction::List => IpcRequest::Roots,
        WatchAction::Add { path } => IpcRequest::WatchAdd {
            path: absolute(path)?,
        },
        WatchAction::Remove { path, forget } => IpcRequest::WatchRemove {
            path: absolute(path)?,
            forget,
        },
    };
    let mut client = ipc_client_for(config_path).await?;
    let resp = client
        .request(request)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if let IpcResponse::Roots(r) = resp {
        if r.roots.is_empty() {
            eprintln!("no watch paths");
        }
        for root in r.roots {
            println!("{root}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, is_dir: bool) -> SearchHitDto {
        SearchHitDto {
            path: path.into(),
            name: String::new(),
            extension: None,
            size_bytes: 7,
            mtime: 1_704_067_200,
            ctime: 0,
            is_dir,
            attrib_bitmask: attrib::HIDDEN | attrib::ARCHIVE,
            archive_path: None,
            snippet: None,
        }
    }

    #[test]
    fn crawl_age_reads_as_time_ago() {
        let now = 1_800_000_000;
        assert_eq!(crawl_age(0, now), "never");
        assert_eq!(crawl_age(now - 42, now), "42s ago");
        assert_eq!(crawl_age(now - 125, now), "2m 5s ago");
        assert_eq!(crawl_age(now - 3 * 3_600 - 60, now), "3h 1m ago");
        assert_eq!(crawl_age(now - 2 * 86_400 - 3_600, now), "2d 1h ago");
        // A clock set back since the crawl.
        assert_eq!(crawl_age(now + 10, now), "0s ago");
    }

    #[test]
    fn hits_become_everything_rows() {
        let file = efu_entry(&hit("/a/b.txt", false));
        assert_eq!(file.size, Some(7));
        assert_eq!(file.mtime, Some(1_704_067_200));
        assert_eq!(file.ctime, None, "0 is unknown, written empty");
        assert_eq!(file.attributes, Some(0x22));
        let folder = efu_entry(&hit("/a", true));
        assert_eq!(folder.size, None);
        assert!(folder.is_dir());
    }
}
