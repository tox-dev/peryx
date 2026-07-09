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

    /// The stable lowercase identifier used in config, routes, the API, and the UI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pypi => "pypi",
            Self::Oci => "oci",
        }
    }
}

/// One ecosystem's user-facing words for peryx's neutral concepts. The neutral name is in each
/// field's doc comment; the value is what a user of that ecosystem calls the same thing.
///
/// peryx's internal model is one neutral vocabulary, but a reader arriving from Docker thinks
/// "registry, repository, tag, blob, push, pull". Each ecosystem crate defines its own `Lexicon`
/// value (this crate stays neutral and never names an ecosystem's words) and registers it, so
/// user-facing surfaces localize a label by looking it up for an index's ecosystem rather than
/// branching on the ecosystem to pick a word. [`Lexicon::NEUTRAL`] is the fallback before any
/// ecosystem registers one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lexicon {
    /// The endpoint a client points at (neutral: index).
    pub server: &'static str,
    /// One named collection of releases (neutral: project).
    pub collection: &'static str,
    /// Many collections (neutral: projects).
    pub collections: &'static str,
    /// The search noun for a collection (neutral: package).
    pub search_noun: &'static str,
    /// One release of a collection (neutral: version).
    pub release: &'static str,
    /// Many releases (neutral: versions).
    pub releases: &'static str,
    /// One stored downloadable piece (neutral: file).
    pub artifact: &'static str,
    /// Many pieces (neutral: files).
    pub artifacts: &'static str,
    /// The verb for fetching (neutral: download).
    pub get: &'static str,
    /// The verb for storing your own (neutral: upload).
    pub put: &'static str,
}

impl Lexicon {
    /// peryx's own neutral vocabulary, used until an ecosystem registers its own words.
    pub const NEUTRAL: Self = Self {
        server: "index",
        collection: "project",
        collections: "projects",
        search_noun: "package",
        release: "version",
        releases: "versions",
        artifact: "file",
        artifacts: "files",
        get: "download",
        put: "upload",
    };
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
    fn test_neutral_lexicon_is_peryxs_own_words() {
        let neutral = super::Lexicon::NEUTRAL;
        assert_eq!(
            (neutral.server, neutral.collection, neutral.release),
            ("index", "project", "version")
        );
        assert_eq!(
            (neutral.artifact, neutral.get, neutral.put),
            ("file", "download", "upload")
        );
    }

    #[test]
    fn test_unknown_ecosystem_reports_the_value() {
        let err = "npm".parse::<Ecosystem>().unwrap_err();
        assert_eq!(err, UnknownEcosystem("npm".to_owned()));
        assert_eq!(err.to_string(), "unknown ecosystem: npm");
    }
}
