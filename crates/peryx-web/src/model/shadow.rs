use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// One page of a virtual repository's resolution for a project: the selected candidate for each
/// filename and the members it shadowed, mirroring the `/+shadow/candidates` response.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiShadowPage {
    pub candidates: Vec<UiShadowCandidate>,
    pub next_cursor: Option<String>,
}

/// A single resolution candidate. The API classifies `source` and `reason` from a closed set, but the
/// client keeps them as strings so an unknown value renders as plain text rather than breaking the
/// page.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiShadowCandidate {
    pub member: String,
    pub source: String,
    pub filename: String,
    #[serde(default)]
    pub digest: Option<String>,
    pub selected: bool,
    #[serde(default)]
    pub reason: Option<String>,
    /// The recorded policy outcome for this filename, absent when policy never evaluated it.
    #[serde(default)]
    pub decision: Option<UiShadowDecision>,
}

/// The allow, deny, or wait outcome a repository's policy recorded for a candidate's filename,
/// mirroring the optional `decision` object the `/+shadow/candidates` response carries.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiShadowDecision {
    pub state: String,
    #[serde(default)]
    pub rule: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    pub evaluated_at_unix: i64,
    #[serde(default)]
    pub next_eligible_at_unix: Option<i64>,
    pub fresh: bool,
}

impl UiShadowDecision {
    /// The outcome as a colour-independent word, marked stale when a newer policy generation has not
    /// re-evaluated it.
    #[must_use]
    pub fn status(&self) -> String {
        let state = match self.state.as_str() {
            "allow" => "Allowed",
            "deny" => "Denied",
            "wait" => "Waiting",
            _ => "Unknown",
        };
        if self.fresh {
            state.to_owned()
        } else {
            format!("Stale {state}")
        }
    }

    /// A stable class suffix so the outcome badge carries meaning beyond colour.
    #[must_use]
    pub fn state_key(&self) -> &str {
        &self.state
    }

    /// When policy last evaluated the filename, as an RFC 3339 UTC instant.
    #[must_use]
    pub fn evaluated_at(&self) -> String {
        format_unix(self.evaluated_at_unix)
    }

    /// The earliest a waiting candidate becomes eligible again, or an em dash when none applies.
    #[must_use]
    pub fn next_eligible_at(&self) -> String {
        self.next_eligible_at_unix.map_or_else(|| "-".to_owned(), format_unix)
    }
}

/// The repository and project an operator asked to inspect, plus the page size. Both names are
/// required: the endpoint explains one project's resolution in one repository at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowInspectionFilters {
    pub repository: String,
    pub project: String,
    pub limit: String,
}

impl Default for ShadowInspectionFilters {
    fn default() -> Self {
        Self {
            repository: String::new(),
            project: String::new(),
            limit: "25".to_owned(),
        }
    }
}

impl ShadowInspectionFilters {
    /// Build the query URL, keeping credentials out of it.
    ///
    /// # Errors
    /// Returns a bounded message when the repository or project is blank.
    pub fn url(&self, cursor: Option<&str>) -> Result<String, String> {
        let repository = self.repository.trim();
        let project = self.project.trim();
        if repository.is_empty() || project.is_empty() {
            return Err("Enter a repository and a project to inspect.".to_owned());
        }
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        serializer.append_pair("repository", repository);
        serializer.append_pair("project", project);
        serializer.append_pair("limit", self.limit.trim());
        if let Some(cursor) = cursor {
            serializer.append_pair("cursor", cursor);
        }
        Ok(format!("/+shadow/candidates?{}", serializer.finish()))
    }
}

fn format_unix(value: i64) -> String {
    let Ok(time) = OffsetDateTime::from_unix_timestamp(value) else {
        return value.to_string();
    };
    time.format(&Rfc3339).unwrap_or_else(|_| value.to_string())
}

impl UiShadowCandidate {
    /// The candidate's outcome as a colour-independent word: the one the repository serves, or a
    /// shadowed loser.
    #[must_use]
    pub fn outcome(&self) -> &'static str {
        if self.selected { "Selected" } else { "Shadowed" }
    }

    /// A stable class suffix so the outcome badge is styled without carrying meaning in colour alone.
    #[must_use]
    pub fn outcome_key(&self) -> &'static str {
        if self.selected { "selected" } else { "shadowed" }
    }

    /// The member class in words: an uploaded artifact or one mirrored from an upstream index.
    #[must_use]
    pub fn source_text(&self) -> &str {
        match self.source.as_str() {
            "hosted" => "hosted upload",
            "cached" => "cached upstream",
            other => other,
        }
    }

    /// Why a shadowed candidate lost, in words; the selected candidate has none.
    #[must_use]
    pub fn reason_text(&self) -> String {
        match self.reason.as_deref() {
            None => "-".to_owned(),
            Some("precedence") => "Higher-precedence member".to_owned(),
            Some("fallback") => "Excluded by fallback policy".to_owned(),
            Some(other) => other.to_owned(),
        }
    }

    /// The digest as shown, or an em dash when the ecosystem does not address the candidate by one.
    #[must_use]
    pub fn digest_text(&self) -> String {
        self.digest.clone().unwrap_or_else(|| "-".to_owned())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/model/shadow/tests.rs"]
mod tests;
