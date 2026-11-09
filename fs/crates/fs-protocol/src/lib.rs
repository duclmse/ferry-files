//! Wire contract between the `fs` client and the `fsd` daemon.

use std::path::PathBuf;

pub mod ipc_client;
pub mod ipc_protocol;

pub fn default_ipc_path() -> PathBuf {
    #[cfg(windows)]
    return PathBuf::from(r"\\.\pipe\fs");
    #[cfg(not(windows))]
    return PathBuf::from("/tmp/fs.sock");
}
