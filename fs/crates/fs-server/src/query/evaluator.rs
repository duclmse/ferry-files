#[cfg(test)]
use crate::store::{CmpOp, SqlCondition};

mod execute;
mod plan;
mod postfilter;
mod types;

#[cfg(test)]
mod tests;

pub use execute::execute_plan;
pub use plan::plan;
pub use postfilter::apply_post_filters;
pub use types::{EvalStrategy, PostFilter, QueryPlan, QueryResult};
