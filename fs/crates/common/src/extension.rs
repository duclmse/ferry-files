//! Contract for `fs` extensions.
//!
//! An extension executable implements this and hands it to
//! `extension_stdio::serve`; the daemon drives the same trait through a child
//! process, so neither side depends on the other.

use std::future::Future;
use std::pin::Pin;

use tokio_util::sync::CancellationToken;

pub type ExtensionFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

pub trait Extension: Send + Sync + 'static {
    /// Key under `[extensions]` in the config, and the name IPC requests use.
    fn name(&self) -> &str;

    /// Full configuration check for `fsd validate-config`. May block on
    /// network probes, so the daemon does not call it at startup.
    fn validate(&self) -> Vec<String>;

    /// Runs until `shutdown` is cancelled and in-flight work has finished.
    fn run(&self, shutdown: CancellationToken) -> ExtensionFuture;

    /// Handles a client command and returns text to show the user. May block;
    /// the daemon calls it from the blocking pool.
    fn command(&self, command: &str) -> anyhow::Result<String>;

    /// Reads the config file again and applies it: the daemon asks after an
    /// API change to `[extensions.<name>]`, since nothing follows the file.
    /// May block; called from the blocking pool. An error keeps the running
    /// settings and is shown to whoever made the change.
    fn reload(&self) -> anyhow::Result<()> {
        anyhow::bail!(
            "{} does not reload its settings; they apply when fsd restarts",
            self.name()
        )
    }
}
