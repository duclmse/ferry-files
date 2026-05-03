use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::error::CommonError;

/// Initialise tracing with JSON format, stdout + rolling daily file.
/// Level controlled by `RUST_LOG` env var (default: `info`).
/// Call once at startup before any async runtime is created.
///
/// The returned `WorkerGuard` must be kept alive for the entire lifetime of
/// the process; dropping it flushes and closes the log file appender.
pub fn init(log_dir: &Path) -> Result<WorkerGuard, CommonError> {
    std::fs::create_dir_all(log_dir).map_err(CommonError::Io)?;

    let file_appender = rolling::daily(log_dir, "app.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(fmt::layer().json().with_writer(std::io::stdout))
        .with(fmt::layer().json().with_writer(non_blocking))
        .try_init()
        .map_err(|e| CommonError::Logger(e.to_string()))?;

    Ok(guard)
}
