pub mod ast;
pub mod date;
pub mod dupes;
pub mod evaluator;
pub mod glob;
pub mod macros;
pub mod parser;
pub mod size;

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::error::Result;
use crate::store::{tantivy_store::TantivyStore, FileStore};

pub use evaluator::{EvalStrategy, QueryResult};

/// Parse and execute a raw query string against the store backends.
pub async fn execute(
    raw_query: &str,
    sqlite: Arc<FileStore>,
    tantivy: Arc<TantivyStore>,
    config: Arc<RwLock<Config>>,
) -> Result<QueryResult> {
    let macros = config.read().await.macros.clone();
    let expr = parser::parse(raw_query, &macros)?;
    let plan = evaluator::plan(&expr, &ast::Modifiers::default())?;
    evaluator::execute_plan(plan, sqlite, tantivy, config).await
}
