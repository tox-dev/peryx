use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoScope {
    All,
    Only(BTreeSet<String>),
}

impl RepoScope {
    #[must_use]
    pub fn permits(&self, repository: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(set) => set.contains(repository),
        }
    }
}

/// Binds cursors to the grant that minted them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryScope {
    repositories: RepoScope,
    fingerprint: String,
}

impl QueryScope {
    /// The fingerprint must change whenever row or field visibility changes.
    #[must_use]
    pub const fn new(repositories: RepoScope, fingerprint: String) -> Self {
        Self {
            repositories,
            fingerprint,
        }
    }

    #[must_use]
    pub const fn repositories(&self) -> &RepoScope {
        &self.repositories
    }

    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}
