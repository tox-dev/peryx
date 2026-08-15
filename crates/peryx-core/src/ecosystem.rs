use core::fmt;
use core::hash::{Hash, Hasher};
use core::str::FromStr;
use std::cmp::Ordering;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone)]
pub struct Ecosystem(EcosystemValue);

#[derive(Debug, Clone)]
enum EcosystemValue {
    Static(&'static str),
    Owned(Arc<str>),
}

impl Ecosystem {
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        Self(EcosystemValue::Static(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            EcosystemValue::Static(value) => value,
            EcosystemValue::Owned(value) => value,
        }
    }
}

impl PartialEq for Ecosystem {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Ecosystem {}

impl PartialOrd for Ecosystem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ecosystem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for Ecosystem {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl fmt::Display for Ecosystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
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
        Ok(Self(EcosystemValue::Owned(Arc::from(value))))
    }
}

impl Serialize for Ecosystem {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Ecosystem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidEcosystem(String);

impl fmt::Display for InvalidEcosystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid ecosystem: {}", self.0)
    }
}

impl std::error::Error for InvalidEcosystem {}

#[cfg(test)]
#[path = "../tests/unit/ecosystem/tests.rs"]
mod tests;
