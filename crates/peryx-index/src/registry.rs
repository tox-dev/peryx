//! The serving index set, swappable at runtime under a lock-free read.
//!
//! An [`IndexSet`] bundles the resolved indexes with the [`RouteResolver`] derived from them, so the
//! two can never drift: a request resolves a route against exactly the indexes that route belongs to.
//! [`IndexRegistry`] holds one behind an [`ArcSwap`], so a management change installs a freshly built
//! set with a single atomic store while every in-flight request keeps reading the snapshot it began
//! with. Reads never take a lock and a swap never waits on a reader, which is what lets a repository
//! change land without a process reload and without stalling a package download behind it.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::index::Index;
use crate::resolve::RouteResolver;

/// A resolved set of indexes and the route resolver built from it.
pub struct IndexSet {
    indexes: Vec<Index>,
    resolver: RouteResolver,
}

impl IndexSet {
    /// Build the set and its route resolver together, so resolution always targets these indexes.
    #[must_use]
    pub fn new(indexes: Vec<Index>) -> Self {
        let resolver = RouteResolver::new(&indexes);
        Self { indexes, resolver }
    }

    /// The indexes in this set, in position order.
    #[must_use]
    pub fn indexes(&self) -> &[Index] {
        &self.indexes
    }

    /// The index at `position`, a virtual-index layer or upload target.
    #[must_use]
    pub fn index_at(&self, position: usize) -> &Index {
        &self.indexes[position]
    }

    /// Resolve `path` (no leading slash) to the index owning the longest route prefix, and the path
    /// remainder after `route/`.
    #[must_use]
    pub fn resolve<'a>(&'a self, path: &'a str) -> Option<(&'a Index, &'a str)> {
        self.resolve_position(path)
            .map(|(position, rest)| (&self.indexes[position], rest))
    }

    /// Like [`resolve`](Self::resolve), returning the resolved index position instead of a borrow.
    #[must_use]
    pub fn resolve_position<'a>(&self, path: &'a str) -> Option<(usize, &'a str)> {
        self.resolver.resolve(path)
    }
}

/// A runtime-swappable [`IndexSet`] with lock-free reads.
pub struct IndexRegistry {
    current: ArcSwap<IndexSet>,
}

impl IndexRegistry {
    /// Install `indexes` as the initial serving set.
    #[must_use]
    pub fn new(indexes: Vec<Index>) -> Self {
        Self {
            current: ArcSwap::from_pointee(IndexSet::new(indexes)),
        }
    }

    /// The current set, owned so a background task can hold it across an await without pinning the
    /// registry. A later swap leaves this snapshot untouched.
    #[must_use]
    pub fn snapshot(&self) -> Arc<IndexSet> {
        self.current.load_full()
    }

    /// Install a freshly built set from `indexes` in one atomic store, returning the set it replaced.
    /// Readers holding the previous snapshot keep serving it until they drop it.
    pub fn replace(&self, indexes: Vec<Index>) -> Arc<IndexSet> {
        self.current.swap(Arc::new(IndexSet::new(indexes)))
    }
}

#[cfg(test)]
#[path = "../tests/unit/registry/tests.rs"]
mod tests;
