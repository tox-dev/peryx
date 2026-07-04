//! The ecosystem axis: which package format an index speaks.
//!
//! An index is a `(role, ecosystem, key)` triple. [`Ecosystem`] is the second axis — a first-class,
//! immutable-at-creation value that selects the format driver. velodex implements `Pypi` today; a new
//! ecosystem is a new variant here plus a sibling `velodex-ecosystem-*` crate implementing
//! [`EcosystemDriver`]. Dispatch on this enum is static, so a single-ecosystem serving path stays
//! branch-free.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

/// The package ecosystem an index serves. Immutable once an index is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Ecosystem {
    /// The Python Package Index: the PEP 503/691 simple API, wheels, and sdists.
    #[default]
    Pypi,
}

impl Ecosystem {
    /// Every known ecosystem, in a stable order, for help text and the UI.
    pub const ALL: &'static [Self] = &[Self::Pypi];

    /// The stable lowercase identifier used in config, routes, the API, and the UI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pypi => "pypi",
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

/// The behavior every ecosystem must provide so the serving layer stays format-agnostic.
///
/// The role model (hosted/proxy/virtual), storage, the journal, aggregation, and the request router
/// are ecosystem-neutral; everything format-specific — index render/parse, artifact identity, the
/// metadata sidecar, the upstream wire protocol, and client setup snippets — lives behind this trait.
/// Its methods are filled in as the serving layer is converted to dispatch through it (the `PyPI`
/// implementation lives in the `velodex-ecosystem-pypi` crate); the one stable method today is the
/// identity of the ecosystem the driver implements.
pub trait EcosystemDriver: Send + Sync + 'static {
    /// The ecosystem this driver implements.
    fn ecosystem(&self) -> Ecosystem;
}
