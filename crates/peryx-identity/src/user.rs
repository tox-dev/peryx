use std::fmt;

use caseless::Caseless as _;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization as _;

/// Storage records this value so Unicode-data upgrades recheck existing identity keys.
pub const USER_NAME_CANONICAL_VERSION: &str = "d145-casefold-16.0.0-normalization-17.0.0";
const _: () = {
    assert!(caseless::UNICODE_VERSION.0 == 16);
    assert!(caseless::UNICODE_VERSION.1 == 0);
    assert!(caseless::UNICODE_VERSION.2 == 0);
    assert!(unicode_normalization::UNICODE_VERSION.0 == 17);
    assert!(unicode_normalization::UNICODE_VERSION.1 == 0);
    assert!(unicode_normalization::UNICODE_VERSION.2 == 0);
};

/// An opaque server-user identifier that remains stable when account attributes change.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(String);

impl UserId {
    #[must_use]
    pub fn random() -> Self {
        Self(format!("usr_{}", uuid::Uuid::new_v4().simple()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn from_stored(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Display names retain trimmed spelling; lookup keys use Unicode canonical caseless matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserName {
    display: String,
    canonical: String,
}

impl UserName {
    /// # Errors
    /// Returns [`UserNameError::Empty`] when `value` contains only whitespace.
    pub fn new(value: &str) -> Result<Self, UserNameError> {
        let display = value.trim();
        if display.is_empty() {
            return Err(UserNameError::Empty);
        }
        Ok(Self {
            display: display.to_owned(),
            canonical: canonicalize(display),
        })
    }

    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }

    #[must_use]
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    #[must_use]
    pub fn with_id_suffix(&self, id: &UserId) -> Self {
        let display = format!("{} ({id})", self.display);
        Self {
            canonical: canonicalize(&display),
            display,
        }
    }
}

fn canonicalize(value: &str) -> String {
    value.chars().nfd().default_case_fold().nfc().collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UserNameError {
    #[error("user display name cannot be empty")]
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserState {
    Active,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerUser {
    pub id: UserId,
    pub name: UserName,
    pub state: UserState,
    pub revision: u64,
    /// A browser session opens only while it carries this value, so advancing it ends every session
    /// sealed before. Records stored before sessions carried an epoch read as 0.
    #[serde(default)]
    pub session_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UserLifecycleChange {
    Created {
        display_name: String,
    },
    AdministratorBootstrapped {
        display_name: String,
    },
    Renamed {
        previous_display_name: String,
        display_name: String,
    },
    Disabled,
    Reactivated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLifecycleEvent {
    pub user_id: UserId,
    pub sequence: u64,
    pub change: UserLifecycleChange,
}
