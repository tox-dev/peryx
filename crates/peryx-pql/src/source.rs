//! The ecosystem-neutral data-source seam.
//!
//! A [`DataSource`] exposes typed domains to the executor and nothing else. This trait is the reason
//! the core stays free of per-ecosystem branching: neutral domains are backed by one source, and each
//! ecosystem crate implements its own source for the domains it owns, without the executor ever
//! learning what an ecosystem's bytes mean.
//!
//! # Read-only by construction
//!
//! Every method here reads. There is deliberately no create, update, or delete — not now and not by
//! extension. PQL is a read surface; writes stay in their existing dedicated, audited endpoints and
//! never route through this trait. A future contributor tempted to add, say, a delete "just for
//! trash" should add it to the trash endpoint, not here: a mutating method on this trait would break
//! the language's central safety property that a query can never change state.

use crate::catalog::DomainSchema;
use crate::error::PqlError;
use crate::scope::QueryScope;
use crate::value::{Row, Value};

/// The cost gate's leading filter, lowered to concrete values, handed to the source so it can narrow
/// the read through its own index.
///
/// The gate admits an unbounded domain only when the query carries an equality or membership on a
/// cheaply-indexed column (constraint 6). Passing that same filter here is what makes admission
/// honest: a source that honors it reads an indexed slice instead of materializing the whole domain
/// and filtering in memory. It stays advisory — the executor re-applies the full predicate over the
/// returned rows — so a source that cannot use it may ignore it without affecting correctness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchFilter {
    /// The indexed column the filter narrows on.
    pub column: &'static str,
    /// The admitted values; the row matches when its `column` cell equals any of them.
    pub values: Vec<Value>,
}

/// A provider of typed, read-only rows for one or more domains.
pub trait DataSource: Send + Sync {
    /// The schema for a domain this source serves, or `None` when it does not serve it.
    fn schema(&self, domain: &str) -> Option<&DomainSchema>;

    /// Read the candidate rows for a domain within the caller's scope.
    ///
    /// Implementations must not mutate any state. The rows returned should already be narrowed to the
    /// caller's authorized repositories where the source can do so cheaply; the executor injects the
    /// same scope again as a predicate, so a source that over-returns is corrected, never leaked.
    ///
    /// When `filter` is `Some`, it is the cost gate's leading indexed filter (see [`FetchFilter`]): a
    /// source SHOULD honor it through its index so an admitted unbounded-domain query does not
    /// materialize the whole domain. Honoring it is advisory — the executor re-applies the full
    /// predicate regardless — so a source without a matching index may ignore it.
    ///
    /// # Errors
    /// Returns [`PqlError::Backend`] when the underlying store cannot answer.
    fn fetch(&self, domain: &str, scope: &QueryScope, filter: Option<&FetchFilter>) -> Result<Vec<Row>, PqlError>;
}
