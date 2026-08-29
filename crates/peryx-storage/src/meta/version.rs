use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionPrecondition {
    Exists,
    Versions(BTreeSet<u64>),
}

impl VersionPrecondition {
    #[must_use]
    pub fn exact(version: u64) -> Self {
        Self::Versions(BTreeSet::from([version]))
    }

    #[must_use]
    pub fn matches(&self, current: Option<u64>) -> bool {
        match self {
            Self::Exists => current.is_some(),
            Self::Versions(versions) => current.is_some_and(|version| versions.contains(&version)),
        }
    }
}

impl From<u64> for VersionPrecondition {
    fn from(version: u64) -> Self {
        Self::exact(version)
    }
}
