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
use crate::value::Row;

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
    /// # Errors
    /// Returns [`PqlError::Backend`] when the underlying store cannot answer.
    fn fetch(&self, domain: &str, scope: &QueryScope) -> Result<Vec<Row>, PqlError>;
}
