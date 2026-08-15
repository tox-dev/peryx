//! The grammar has bounded pagination, one declared join, and fixed aggregates. The executor applies
//! [`QueryScope`] before filtering, aggregation, ordering, and paging. [`DataSource`] exposes no write
//! operation.

pub mod ast;
pub mod catalog;
pub mod cursor;
pub mod error;
pub mod eval;
pub mod execute;
pub mod parse;
pub mod plan;
pub mod scope;
pub mod source;
pub mod value;

#[cfg(test)]
#[path = "../tests/unit/tests.rs"]
mod tests;

pub use ast::{Aggregate, AggregateFunc, AggregateTerm, Ast, CompareOp, Join, Literal, OrderKey, Predicate, Selection};
pub use catalog::{Column, DomainAuth, DomainSchema, FieldClass, Indexability};
pub use cursor::{decode, encode};
pub use error::{PqlError, StatusClass};
pub use eval::{evaluate, literal_value};
pub use execute::{Page, execute};
pub use parse::{MAX_PREDICATE_DEPTH, MAX_QUERY_BYTES, Params, bind, parse};
pub use plan::{DEFAULT_LIMIT, MAX_LIMIT, OutputColumn, Plan, gate_join, leading_filter, plan, validate};
pub use scope::{QueryScope, RepoScope};
pub use source::{DataSource, FetchFilter};
pub use value::{Row, Value, ValueType};

/// # Errors
/// Returns the first parser, binding, planning, cursor, or source error.
pub fn run(
    text: &str,
    params: &Params,
    scope: &QueryScope,
    cursor_text: Option<&str>,
    source: &dyn DataSource,
) -> Result<Page, PqlError> {
    let ast = bind(parse(text)?, params)?;
    execute(&ast, scope, cursor_text, source)
}
