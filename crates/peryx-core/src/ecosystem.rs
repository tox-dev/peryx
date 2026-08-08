//! Opaque package-ecosystem identity.

use core::fmt;
use core::str::FromStr;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Stable ecosystem identity used in configuration, routes, storage, and registries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ecosystem(&'static str);

impl Ecosystem {
    /// Define an ecosystem identity in its implementing crate.
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    /// The stable lowercase identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for Ecosystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl FromStr for Ecosystem {
    type Err = InvalidEcosystem;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(InvalidEcosystem(value.to_owned()));
        }
        let identities = INTERNED.get_or_init(|| Mutex::new(HashSet::new()));
        let mut identities = identities.lock().expect("ecosystem identity lock poisoned");
        if let Some(identity) = identities.get(value) {
            return Ok(Self(identity));
        }
        let identity = Box::leak(value.to_owned().into_boxed_str());
        identities.insert(identity);
        drop(identities);
        Ok(Self(identity))
    }
}

impl Serialize for Ecosystem {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for Ecosystem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// An ecosystem identity was empty or contained unsupported characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidEcosystem(String);

impl fmt::Display for InvalidEcosystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid ecosystem: {}", self.0)
    }
}

impl std::error::Error for InvalidEcosystem {}

static INTERNED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();

#[cfg(test)]
#[path = "../tests/unit/ecosystem/tests.rs"]
mod tests;
