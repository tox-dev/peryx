//! The webhook event model and the JSON payload signed and delivered for each event.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WebhookEventKind {
    Upload,
    Yank,
    Unyank,
    Delete,
    Restore,
    Promote,
    ProjectStatus,
    Management,
}

impl WebhookEventKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Yank => "yank",
            Self::Unyank => "unyank",
            Self::Delete => "delete",
            Self::Restore => "restore",
            Self::Promote => "promote",
            Self::ProjectStatus => "project-status",
            Self::Management => "management",
        }
    }

    pub(super) fn parse(name: &str) -> Option<Self> {
        match name {
            "upload" => Some(Self::Upload),
            "yank" => Some(Self::Yank),
            "unyank" => Some(Self::Unyank),
            "delete" => Some(Self::Delete),
            "restore" => Some(Self::Restore),
            "promote" => Some(Self::Promote),
            "project-status" => Some(Self::ProjectStatus),
            "management" => Some(Self::Management),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEvent {
    pub kind: WebhookEventKind,
    pub created_at_unix: i64,
    pub index: String,
    pub route: String,
    pub hosted_index: String,
    pub project: String,
    pub version: Option<String>,
    pub filename: Option<String>,
    pub digest: Option<String>,
    pub count: usize,
    pub actor: Option<String>,
    pub request_id: Option<String>,
}

impl WebhookEvent {
    pub(super) fn payload(&self) -> WebhookPayload<'_> {
        WebhookPayload {
            event: self.kind.as_str(),
            created_at: self.created_at_unix,
            index: &self.index,
            route: &self.route,
            hosted_index: &self.hosted_index,
            project: &self.project,
            version: self.version.as_deref(),
            file: self.filename.as_deref().map(|filename| WebhookFile {
                filename,
                sha256: self.digest.as_deref(),
            }),
            count: self.count,
            actor: self.actor.as_deref(),
            request_id: self.request_id.as_deref(),
        }
    }
}

#[derive(Serialize)]
pub(super) struct WebhookPayload<'a> {
    event: &'static str,
    created_at: i64,
    index: &'a str,
    route: &'a str,
    hosted_index: &'a str,
    project: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<WebhookFile<'a>>,
    count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<&'a str>,
}

#[derive(Serialize)]
struct WebhookFile<'a> {
    filename: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_names_roundtrip() {
        for (kind, name) in [
            (WebhookEventKind::Upload, "upload"),
            (WebhookEventKind::Yank, "yank"),
            (WebhookEventKind::Unyank, "unyank"),
            (WebhookEventKind::Delete, "delete"),
            (WebhookEventKind::Restore, "restore"),
            (WebhookEventKind::Promote, "promote"),
            (WebhookEventKind::ProjectStatus, "project-status"),
            (WebhookEventKind::Management, "management"),
        ] {
            assert_eq!(kind.as_str(), name);
            assert_eq!(WebhookEventKind::parse(name), Some(kind));
        }
    }
}
