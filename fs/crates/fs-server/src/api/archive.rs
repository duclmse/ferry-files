//! Archive Service: the registry of indexed ZIPs and, with the `rest`
//! feature, its HTTP routes.
//!
//! Routes (mounted only when archive_service.enabled = true):
//!   GET  /archive - list all registered archives
//!   GET  /archive/{id}/index - raw .zidx binary
//!   GET  /archive/{id}/dir?node=`<u32>` - directory listing
//!   GET  /archive/{id}/file?node=`<u32>` - stream file content; `Range:` → 206
//!        for stored entries and deflate entries with a `.dzx` sidecar
//!   GET  /archive/{id}/search?q=&limit=&offset= - archive-scoped search

#[cfg(feature = "rest")]
mod http;
mod registry;

#[cfg(feature = "rest")]
pub use http::router;
pub use registry::{archive_id, ArchiveRegistry, ArchiveSummary};
