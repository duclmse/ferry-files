//! Contract between the `fs` daemon and in-process extensions.
//!
//! It lives in `common` so extension crates and `fs-server` can both depend on
//! it without depending on each other in a cycle.

use std::future::Future;
use std::pin::Pin;

use tokio_util::sync::CancellationToken;

pub type ExtensionFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

pub trait Extension: Send + Sync + 'static {
    /// Key under `[extensions]` in the config, and the name IPC requests use.
    fn name(&self) -> &'static str;

    /// Full configuration check for `fsd validate-config`. May block on
    /// network probes, so the daemon does not call it at startup.
    fn validate(&self) -> Vec<String>;

    /// Runs until `shutdown` is cancelled and in-flight work has finished.
    fn run(&self, shutdown: CancellationToken) -> ExtensionFuture;

    /// Handles a client command and returns text to show the user. May block;
    /// the daemon calls it from the blocking pool.
    fn command(&self, command: &str) -> anyhow::Result<String>;
}
