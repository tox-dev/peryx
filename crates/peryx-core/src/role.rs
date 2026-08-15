use core::fmt;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Cached,
    /// Stores writes and owns the resulting artifacts.
    Hosted,
    Virtual,
}

impl Role {
    /// Stable display order.
    pub const ALL: &'static [Self] = &[Self::Cached, Self::Hosted, Self::Virtual];

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
#[path = "../tests/unit/role/tests.rs"]
mod tests;
