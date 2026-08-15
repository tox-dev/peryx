use core::borrow::Borrow;
use core::fmt;

use serde::{Deserialize, Serialize};

macro_rules! coordinate {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Box<str>);

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<Box<str>>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[must_use]
            pub fn into_boxed_str(self) -> Box<str> {
                self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl From<Box<str>> for $name {
            fn from(value: Box<str>) -> Self {
                Self::new(value)
            }
        }
    };
}

coordinate!(RepositoryKey, "A configured repository.");
coordinate!(ResourceKey, "An ecosystem-normalized resource.");
coordinate!(GroupKey, "An ecosystem-defined accounting or reporting group.");
coordinate!(ArtifactKey, "An ecosystem-defined stored or served artifact.");

#[cfg(test)]
#[path = "../tests/unit/resource/tests.rs"]
mod tests;
