//! PQL, the Peryx Query Language: a small, read-only, non-Turing-complete structured query language
//! over peryx's typed domains.
//!
//! PQL is a single query surface over the operational state peryx already exposes through a patchwork
//! of typed endpoints - usage, policy decisions, trash, retention, quota, revocations, and (later)
//! per-ecosystem package metadata. It parses a textual query, validates and costs it against a static
//! catalog, injects the caller's authorization scope structurally, and evaluates it over a
//! [`DataSource`], returning typed rows.
//!
//! # Non-Turing-complete
//!
//! The language expresses selection, filtering, ordering, bounded pagination, one declared join, and
//! a fixed set of aggregates - nothing else. There are no loops, no recursion, no user-defined
//! functions, and no arithmetic beyond comparison. Evaluation is linear in the number of candidate
//! rows and its cost is bounded before execution.
//!
//! # Read-only by construction
//!
//! PQL has no write, update, or delete surface, and is designed so it never grows one. The
//! [`DataSource`] trait exposes only reads; the AST has no mutation node; the evaluator has no
//! side-effecting operator. Writes stay in their existing dedicated, audited endpoints. This is a
//! load-bearing safety property, not a convention: a mutating query language is the single largest
//! footgun this design exists to avoid, so a contributor tempted to add a delete "just for trash"
//! must add it to the trash endpoint, never here.
//!
//! # Authorization is structural
//!
//! The caller never writes their own scope. The wire layer resolves the caller into one
//! [`QueryScope`], and the executor injects it as a mandatory predicate the query text can neither
//! name nor remove, applied before ordering and paging so pagination and counts never leak an
//! unauthorized row. A broader grant only widens the injected set; it never changes what a query
//! means. Column-level visibility is decided by each column's classification, checked by the wire
//! layer against the same field-classification primitive every other endpoint uses.

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
mod tests;

pub use ast::Ast;
pub use catalog::{Column, DomainAuth, DomainSchema, FieldClass, Indexability};
pub use error::{PqlError, StatusClass};
pub use execute::{Page, execute};
pub use parse::{Params, bind, parse};
pub use plan::{OutputColumn, Plan, plan};
pub use scope::{QueryScope, RepoScope};
pub use source::{DataSource, FetchFilter};
pub use value::{Row, Value, ValueType};

/// Parse, bind, and execute a textual query in one call.
///
/// This is the entry point the wire layer uses: it turns query text and out-of-band parameters into
/// one page of typed rows, with the caller's scope injected.
///
/// # Errors
/// Propagates every stage's error: parsing, parameter binding, validation, cost, cursor, and backend
/// failures, as one [`PqlError`].
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
