use std::fmt;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization as _;

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

/// Display names retain trimmed spelling; lookup keys use lowercase NFC.
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
            canonical: display.to_lowercase().nfc().collect(),
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
            canonical: display.to_lowercase().nfc().collect(),
            display,
        }
    }
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
