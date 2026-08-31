use serde::{Deserialize, Serialize};

use super::SimpleError;

/// The highest Simple API version peryx advertises, for a page that carries every PEP 700 guarantee.
pub const API_VERSION: &str = "1.4";
/// The version peryx falls back to when it cannot guarantee PEP 700's `versions` and per-file `size`:
/// PEP 691, which mandates neither. PEP 700's additions start at 1.1, so anything below it is `1.0`.
pub const API_VERSION_BASE: &str = "1.0";
const API_MAJOR: u64 = 1;
/// The first minor version whose payload guarantees `versions` and per-file `size` (PEP 700).
const PEP700_MINOR: u64 = 1;

/// The `meta` object shared by both response kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Meta {
    #[serde(rename = "api-version")]
    pub api_version: &'static str,
    #[serde(skip)]
    pub project_status: Option<String>,
    #[serde(skip)]
    pub project_status_reason: Option<String>,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            api_version: API_VERSION,
            project_status: None,
            project_status_reason: None,
        }
    }
}

impl Meta {
    /// Build served metadata from upstream metadata after checking Simple API compatibility.
    ///
    /// # Errors
    /// Returns [`SimpleError`] when the upstream advertises an invalid or unsupported API version.
    pub fn from_upstream(
        api_version: Option<&str>,
        project_status: Option<String>,
        project_status_reason: Option<String>,
    ) -> Result<Self, SimpleError> {
        let api_version = served_version(api_version)?;
        if let Some(status) = project_status.as_deref() {
            validate_project_status(status)?;
        }
        Ok(Self {
            api_version,
            project_status,
            project_status_reason,
        })
    }

    /// Whether the served version carries PEP 700's `versions` and per-file `size` guarantees. The
    /// derivation collapses every upstream minor to one of two values, so the ceiling means 1.1+.
    #[must_use]
    pub fn promises_pep700(&self) -> bool {
        self.api_version == API_VERSION
    }

    #[must_use]
    pub fn status(&self) -> ProjectStatus {
        self.project_status
            .as_deref()
            .and_then(ProjectStatus::from_marker)
            .unwrap_or(ProjectStatus::Active)
    }

    pub(super) fn project_status_object(&self) -> Option<ProjectStatusObject<'_>> {
        (self.project_status.is_some() || self.project_status_reason.is_some()).then_some(ProjectStatusObject {
            status: self.project_status.as_deref(),
            reason: self.project_status_reason.as_deref(),
        })
    }
}

#[derive(Serialize)]
pub struct ProjectStatusObject<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<&'a str>,
}

#[derive(Deserialize)]
pub(super) struct IncomingProjectStatus {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

impl IncomingProjectStatus {
    pub(super) fn into_parts(self) -> (Option<String>, Option<String>) {
        (self.status, self.reason)
    }
}

#[derive(Default, Deserialize)]
pub(super) struct IncomingMeta {
    #[serde(rename = "api-version", default)]
    api_version: Option<String>,
    #[serde(rename = "project-status", default)]
    project_status: Option<String>,
    #[serde(rename = "project-status-reason", default)]
    project_status_reason: Option<String>,
}

impl IncomingMeta {
    pub(super) fn into_meta(self) -> Result<Meta, SimpleError> {
        Meta::from_upstream(
            self.api_version.as_deref(),
            self.project_status,
            self.project_status_reason,
        )
    }

    pub(super) fn into_detail_meta(self, project_status: Option<IncomingProjectStatus>) -> Result<Meta, SimpleError> {
        let (project_status, project_status_reason) = project_status.map_or(
            (self.project_status, self.project_status_reason),
            IncomingProjectStatus::into_parts,
        );
        Meta::from_upstream(self.api_version.as_deref(), project_status, project_status_reason)
    }
}

/// The version peryx advertises for an upstream that declared `version`: its own ceiling once the
/// upstream promises PEP 700's `versions`/`size` (minor >= 1), otherwise PEP 691's base. An upstream
/// that advertises nothing (a bare PEP 503 index) promises neither, so it maps to the base too.
fn served_version(version: Option<&str>) -> Result<&'static str, SimpleError> {
    let Some(version) = version else {
        return Ok(API_VERSION_BASE);
    };
    let Some((major, minor)) = version.split_once('.') else {
        return Err(SimpleError::InvalidApiVersion(version.to_owned()));
    };
    let major = major
        .parse::<u64>()
        .map_err(|_| SimpleError::InvalidApiVersion(version.to_owned()))?;
    if major != API_MAJOR {
        return Err(SimpleError::UnsupportedApiVersion(version.to_owned()));
    }
    let minor = minor
        .parse::<u64>()
        .map_err(|_| SimpleError::InvalidApiVersion(version.to_owned()))?;
    Ok(if minor >= PEP700_MINOR {
        API_VERSION
    } else {
        API_VERSION_BASE
    })
}

pub(super) fn validate_project_status(status: &str) -> Result<(), SimpleError> {
    ProjectStatus::from_marker(status)
        .map(|_| ())
        .ok_or_else(|| SimpleError::InvalidProjectStatus(status.to_owned()))
}

/// The standardized project status markers and their serving policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStatus {
    Active,
    Archived,
    Quarantined,
    Deprecated,
}

impl ProjectStatus {
    #[must_use]
    pub fn from_marker(status: &str) -> Option<Self> {
        match status {
            "active" => Some(Self::Active),
            "archived" => Some(Self::Archived),
            "quarantined" => Some(Self::Quarantined),
            "deprecated" => Some(Self::Deprecated),
            _ => None,
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Quarantined => "quarantined",
            Self::Deprecated => "deprecated",
        }
    }

    #[must_use]
    pub const fn allows_uploads(self) -> bool {
        matches!(self, Self::Active | Self::Deprecated)
    }

    #[must_use]
    pub const fn offers_downloads(self) -> bool {
        !matches!(self, Self::Quarantined)
    }

    #[must_use]
    pub const fn severity(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::Deprecated => 1,
            Self::Archived => 2,
            Self::Quarantined => 3,
        }
    }
}
