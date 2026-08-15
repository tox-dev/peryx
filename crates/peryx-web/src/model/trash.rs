use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiTrashPage {
    pub trash: Vec<UiTrashRecord>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiTrashRecord {
    pub ecosystem: String,
    pub repository: String,
    #[serde(alias = "name")]
    pub resource: String,
    #[serde(alias = "reference")]
    pub artifact: Option<String>,
    pub digest: Option<String>,
    pub reason: Option<String>,
    /// Present only when the role filter grants the caller actor visibility.
    #[serde(default)]
    pub actor: Option<String>,
    pub deleted_at_unix: i64,
    pub deadline_unix: i64,
    pub state: String,
    pub restorable: bool,
}

impl UiTrashRecord {
    #[must_use]
    pub fn state_label(&self) -> &'static str {
        match self.state.as_str() {
            "restorable" => "Restorable",
            "expired" => "Expired",
            "other" => "Other",
            _ => "Unknown",
        }
    }

    #[must_use]
    pub fn deleted_at(&self) -> String {
        format_unix(self.deleted_at_unix)
    }

    #[must_use]
    pub fn deadline_at(&self) -> String {
        format_unix(self.deadline_unix)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashFilters {
    pub repository: String,
    pub ecosystem: String,
    pub state: String,
    pub limit: String,
}

impl Default for TrashFilters {
    fn default() -> Self {
        Self {
            repository: String::new(),
            ecosystem: String::new(),
            state: String::new(),
            limit: "25".to_owned(),
        }
    }
}

impl TrashFilters {
    #[must_use]
    pub fn url(&self, cursor: Option<&str>) -> String {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in [
            ("repository", self.repository.trim()),
            ("ecosystem", self.ecosystem.trim()),
            ("state", self.state.trim()),
        ] {
            if !value.is_empty() {
                serializer.append_pair(name, value);
            }
        }
        serializer.append_pair("limit", self.limit.trim());
        if let Some(cursor) = cursor {
            serializer.append_pair("cursor", cursor);
        }
        format!("/+trash?{}", serializer.finish())
    }
}

fn format_unix(value: i64) -> String {
    let Ok(time) = OffsetDateTime::from_unix_timestamp(value) else {
        return value.to_string();
    };
    time.format(&Rfc3339).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
#[path = "../../tests/unit/model/trash/tests.rs"]
mod tests;
