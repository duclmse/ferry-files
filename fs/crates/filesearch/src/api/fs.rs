//! Filesystem operations API.
//!
//! Routes:
//!   HEAD   /fs/read?path= - file metadata as response headers
//!   GET    /fs/read?path= - stream file; `Range:` header → 206 Partial Content
//!   PUT    /fs/write?path= - write/overwrite file content (request body → file)
//!   POST   /fs/copy[?stream=true] - copy file or directory { "from", "to" }
//!   POST   /fs/move[?stream=true] - move / rename          { "from", "to" }
//!   DELETE /fs/delete?path=[&stream=true] - delete file or directory
//!   POST   /fs/mkdir?path= - create directory (with parents)
//!
//! **Streaming progress** (`?stream=true`): copy/move/delete return
//! `text/event-stream` (SSE).  Each event is a JSON object:
//!   `{ "processed": N, "total": N, "current": "path", "done": bool, "error"?: "..." }`
//!
//! Paths must be absolute.  Relative paths and `..` components are rejected
//! with 400 to prevent directory traversal.

use axum::{
    routing::{delete, get, post, put},
    Router,
};

use crate::api::rest::AppState;

mod common;
mod drives;
mod read;
mod write;
#[cfg(test)]
mod tests;

use drives::drives_handler;
use read::{head_handler, read_handler};
use write::{copy_handler, delete_handler, mkdir_handler, move_handler, write_handler};

pub use common::ProgressEvent;
pub use drives::DriveInfo;
pub(crate) use common::parse_range;

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/fs/read",   get(read_handler).head(head_handler))
        .route("/fs/write",  put(write_handler))
        .route("/fs/copy",   post(copy_handler))
        .route("/fs/move",   post(move_handler))
        .route("/fs/delete", delete(delete_handler))
        .route("/fs/mkdir",  post(mkdir_handler))
        .route("/fs/drives", get(drives_handler))
        .with_state(state)
}
