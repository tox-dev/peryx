//! The resolved authorization scope the evaluator injects.
//!
//! The caller never writes their own scope. The wire layer resolves the caller — across both the
//! token/ACL model and the RBAC role model — into one [`QueryScope`], and the evaluator injects it
//! as a mandatory predicate the query text can neither name nor remove. A broader grant only widens
//! the injected set; it never changes what a query means.

use std::collections::BTreeSet;

/// Which repositories a caller may read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoScope {
    /// Operator-wide: every repository is in scope.
    All,
    /// Exactly this set of repositories.
    Only(BTreeSet<String>),
}

impl RepoScope {
    /// Whether a row's repository is within scope.
    #[must_use]
    pub fn permits(&self, repository: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(set) => set.contains(repository),
        }
    }
}

/// The caller's resolved read scope, plus a stable fingerprint used to bind a cursor to the grant it
/// was minted under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryScope {
    repositories: RepoScope,
    fingerprint: String,
}

impl QueryScope {
    /// Resolve a scope from its repository set and a caller fingerprint.
    ///
    /// The fingerprint must be a stable encoding of every input that decides which rows and fields
    /// the caller may see: the authorized repositories and the caller's classification level. Two
    /// callers with different visibility must produce different fingerprints, so a cursor cannot be
    /// replayed under a changed grant.
    #[must_use]
    pub const fn new(repositories: RepoScope, fingerprint: String) -> Self {
        Self {
            repositories,
            fingerprint,
        }
    }

    /// The repositories in scope.
    #[must_use]
    pub const fn repositories(&self) -> &RepoScope {
        &self.repositories
    }

    /// The grant fingerprint bound into any cursor this scope mints.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}
