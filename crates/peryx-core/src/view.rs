use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RenderedDescription {
    pub html: String,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BrowsePage {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breadcrumbs: Vec<BrowseLink>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub badges: Vec<BrowseBadge>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<BrowseSection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<UiAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BrowseLink {
    pub label: String,
    pub href: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BrowseBadge {
    pub label: String,
    pub class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowseSection {
    Markup {
        heading: String,
        html: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notice: Option<String>,
    },
    Properties {
        heading: String,
        entries: Vec<BrowseProperty>,
    },
    Links {
        heading: String,
        entries: Vec<BrowseLink>,
        empty: String,
    },
    Table {
        heading: String,
        columns: Vec<String>,
        rows: Vec<BrowseRow>,
        empty: String,
    },
    Content {
        heading: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        offset: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<BrowseLink>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BrowseProperty {
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BrowseRow {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cells: Vec<BrowseCell>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub badges: Vec<BrowseBadge>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<UiAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BrowseCell {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(default)]
    pub code: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiActionMethod {
    Put,
    Post,
    Delete,
}

impl UiActionMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Put => "PUT",
            Self::Post => "POST",
            Self::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiAction {
    pub label: String,
    pub method: UiActionMethod,
    pub endpoint: String,
    #[serde(default)]
    pub destructive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiArtifactSource {
    Hosted,
    Proxy,
    Generated,
}

impl UiArtifactSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hosted => "hosted",
            Self::Proxy => "proxy",
            Self::Generated => "generated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiByteAvailability {
    Local,
    RemoteOnly,
    Unavailable,
}

impl UiByteAvailability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::RemoteOnly => "remote_only",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiOperationStatus {
    Pending,
    Published,
    Failed,
    Expired,
}

impl UiOperationStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Published => "published",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }

    #[must_use]
    pub const fn derive(published: bool, failed: bool, expiry: Option<i64>, now: i64) -> Self {
        if published {
            Self::Published
        } else if failed {
            Self::Failed
        } else if let Some(expiry) = expiry {
            if now >= expiry { Self::Expired } else { Self::Pending }
        } else {
            Self::Pending
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/view/tests.rs"]
mod tests;
