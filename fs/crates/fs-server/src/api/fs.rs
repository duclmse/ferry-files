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
//! Routes that change the filesystem require `Authorization: Bearer <token>`
//! and are refused entirely until `api.auth_token_hash` is configured.
//!
//! Paths must be absolute.  Relative paths and `..` components are rejected
//! with 400 to prevent directory traversal.

use axum::{
    middleware::from_fn_with_state,
    routing::{delete, get, post, put},
    Router,
};

use crate::api::auth::require_token;
use crate::api::rest::AppState;

mod common;
mod drives;
mod read;
#[cfg(test)]
mod tests;
mod write;

use drives::drives_handler;
use read::{head_handler, read_handler};
use write::{copy_handler, delete_handler, mkdir_handler, move_handler, write_handler};

pub(crate) use common::parse_range;
pub use common::ProgressEvent;
pub use drives::DriveInfo;

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(state: AppState) -> Router<AppState> {
    let mutating = Router::new()
        .route("/fs/write", put(write_handler))
        .route("/fs/copy", post(copy_handler))
        .route("/fs/move", post(move_handler))
        .route("/fs/delete", delete(delete_handler))
        .route("/fs/mkdir", post(mkdir_handler))
        .route_layer(from_fn_with_state(state.clone(), require_token));

    Router::new()
        .route("/fs/read", get(read_handler).head(head_handler))
        .route("/fs/drives", get(drives_handler))
        .merge(mutating)
        .with_state(state)
}
