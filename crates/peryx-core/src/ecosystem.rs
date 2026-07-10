//! The ecosystem axis: which package format an index speaks.
//!
//! An index is a `(role, ecosystem, key)` triple. [`Ecosystem`] is the second axis: a first-class,
//! immutable-at-creation value that selects the format driver. peryx implements `Pypi` today; a new
//! ecosystem is a new variant here plus a sibling `peryx-ecosystem-*` crate that registers a serving
//! driver. Dispatch on this enum is static, so a single-ecosystem serving path stays branch-free.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

/// The package ecosystem an index serves. Immutable once an index is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ecosystem {
    /// The Python Package Index: the PEP 503/691 simple API, wheels, and sdists.
    #[default]
    Pypi,
    /// An OCI/Docker registry: the distribution-spec `/v2/` API, manifests, and content-addressed blobs.
    Oci,
}

impl Ecosystem {
    /// Every known ecosystem, in a stable order, for help text and the UI.
    pub const ALL: &'static [Self] = &[Self::Pypi, Self::Oci];

    /// How many ecosystems exist, so a registry keyed by one is a fixed-size array.
    pub const COUNT: usize = Self::ALL.len();

    /// This ecosystem's slot in such a registry. Discriminants follow [`ALL`](Self::ALL), which
    /// `test_slots_match_the_declaration_order` pins, so the lookup is an array index rather than a
    /// hash.
    #[must_use]
    pub const fn slot(self) -> usize {
        self as usize
    }

    /// The stable lowercase identifier used in config, routes, the API, and the UI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pypi => "pypi",
            Self::Oci => "oci",
        }
    }
}

impl fmt::Display for Ecosystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string did not name a known [`Ecosystem`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownEcosystem(pub String);

impl fmt::Display for UnknownEcosystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown ecosystem: {}", self.0)
    }
}

impl std::error::Error for UnknownEcosystem {}

impl FromStr for Ecosystem {
    type Err = UnknownEcosystem;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|candidate| candidate.as_str() == value)
            .ok_or_else(|| UnknownEcosystem(value.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::{Ecosystem, UnknownEcosystem};

    #[test]
    fn test_ecosystem_string_forms_and_parsing() {
        assert_eq!(Ecosystem::default(), Ecosystem::Pypi);
        assert_eq!(Ecosystem::Pypi.as_str(), "pypi");
        assert_eq!(Ecosystem::Pypi.to_string(), "pypi");
        assert_eq!(Ecosystem::Oci.as_str(), "oci");
        assert_eq!(Ecosystem::Oci.to_string(), "oci");
        assert_eq!(Ecosystem::ALL, &[Ecosystem::Pypi, Ecosystem::Oci]);
        assert_eq!("pypi".parse::<Ecosystem>().unwrap(), Ecosystem::Pypi);
        assert_eq!("oci".parse::<Ecosystem>().unwrap(), Ecosystem::Oci);
    }

    #[test]
    fn test_slots_match_the_declaration_order() {
        for (expected, ecosystem) in Ecosystem::ALL.iter().enumerate() {
            assert_eq!(ecosystem.slot(), expected, "slot of {ecosystem} moved");
        }
        assert_eq!(Ecosystem::COUNT, Ecosystem::ALL.len());
    }

    #[test]
    fn test_unknown_ecosystem_reports_the_value() {
        let err = "npm".parse::<Ecosystem>().unwrap_err();
        assert_eq!(err, UnknownEcosystem("npm".to_owned()));
        assert_eq!(err.to_string(), "unknown ecosystem: npm");
    }
}
