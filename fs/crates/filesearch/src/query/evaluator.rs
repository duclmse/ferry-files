#[cfg(test)]
use crate::store::{CmpOp, SqlCondition};

mod types;
mod plan;
mod postfilter;
mod execute;

#[cfg(test)]
mod tests;

pub use types::{EvalStrategy, PostFilter, QueryPlan, QueryResult};
pub use plan::plan;
pub use postfilter::apply_post_filters;
pub use execute::execute_plan;
