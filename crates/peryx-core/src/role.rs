//! The role axis: what an index *does*, independent of the ecosystem it speaks.
//!
//! An index is a `(role, ecosystem, key)` triple. [`Role`] is the first axis. It carries no payload,
//! so it is the shape a neutral surface matches on: metric families scope themselves to the roles
//! that emit them, and the render layer gates counters by it. The runtime index type pairs each role
//! with what that role needs (a proxy's upstream client, a hosted store's upload policy, a virtual
//! index's members) and lives where those dependencies do.

use core::fmt;

use serde::Serialize;

/// What an index does with the artifacts it serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Reads through to an upstream index and caches what it fetches.
    Cached,
    /// Stores what is uploaded to it, and is the authority for those artifacts.
    Hosted,
    /// Aggregates other indexes of the same ecosystem behind one route.
    Virtual,
}

impl Role {
    /// Every role, in a stable order, for help text and the UI.
    pub const ALL: &'static [Self] = &[Self::Cached, Self::Hosted, Self::Virtual];

    /// The stable lowercase identifier used in config, the API, and the UI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cached => "cached",
            Self::Hosted => "hosted",
            Self::Virtual => "virtual",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::Role;

    #[test]
    fn test_role_string_forms_are_stable() {
        assert_eq!(Role::Cached.as_str(), "cached");
        assert_eq!(Role::Hosted.to_string(), "hosted");
        assert_eq!(Role::Virtual.as_str(), "virtual");
        assert_eq!(Role::ALL, &[Role::Cached, Role::Hosted, Role::Virtual]);
    }

    #[test]
    fn test_role_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Role::Virtual).unwrap(), "\"virtual\"");
    }
}
