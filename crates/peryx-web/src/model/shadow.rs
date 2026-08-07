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
mod tests {
    use rstest::rstest;

    use super::{UiShadowCandidate, UiShadowDecision};

    fn candidate(source: &str, selected: bool, reason: Option<&str>) -> UiShadowCandidate {
        UiShadowCandidate {
            member: "hosted".to_owned(),
            source: source.to_owned(),
            filename: "flask-1.0.bin".to_owned(),
            digest: Some("sha256:abc".to_owned()),
            selected,
            reason: reason.map(str::to_owned),
            decision: None,
        }
    }

    fn decision(state: &str, fresh: bool, next_eligible_at_unix: Option<i64>) -> UiShadowDecision {
        UiShadowDecision {
            state: state.to_owned(),
            rule: Some("blocked-project".to_owned()),
            reason: Some("project is blocked".to_owned()),
            evaluated_at_unix: 0,
            next_eligible_at_unix,
            fresh,
        }
    }

    #[rstest]
    #[case::selected(true, None, "Selected", "selected")]
    #[case::shadowed(false, Some("precedence"), "Shadowed", "shadowed")]
    fn test_outcome_names_each_side(
        #[case] selected: bool,
        #[case] reason: Option<&str>,
        #[case] outcome: &str,
        #[case] key: &str,
    ) {
        let candidate = candidate("hosted", selected, reason);
        assert_eq!((candidate.outcome(), candidate.outcome_key()), (outcome, key));
    }

    #[rstest]
    #[case::hosted("hosted", "hosted upload")]
    #[case::cached("cached", "cached upstream")]
    #[case::unknown("mirror", "mirror")]
    fn test_source_text_states_each_member_class(#[case] source: &str, #[case] text: &str) {
        assert_eq!(candidate(source, true, None).source_text(), text);
    }

    #[rstest]
    #[case::selected(None, "-")]
    #[case::precedence(Some("precedence"), "Higher-precedence member")]
    #[case::fallback(Some("fallback"), "Excluded by fallback policy")]
    #[case::unknown(Some("other"), "other")]
    fn test_reason_text_explains_each_exclusion(#[case] reason: Option<&str>, #[case] text: &str) {
        assert_eq!(candidate("cached", reason.is_none(), reason).reason_text(), text);
    }

    #[test]
    fn test_digest_text_falls_back_to_a_dash() {
        let mut candidate = candidate("hosted", true, None);
        candidate.digest = None;
        assert_eq!(candidate.digest_text(), "-");
    }

    #[rstest]
    #[case::allow("allow", true, "Allowed")]
    #[case::deny("deny", true, "Denied")]
    #[case::wait("wait", true, "Waiting")]
    #[case::unknown("skip", true, "Unknown")]
    #[case::stale("deny", false, "Stale Denied")]
    fn test_decision_status_names_each_outcome(#[case] state: &str, #[case] fresh: bool, #[case] status: &str) {
        assert_eq!(decision(state, fresh, None).status(), status);
    }

    #[test]
    fn test_decision_state_key_exposes_the_raw_state_for_styling() {
        assert_eq!(decision("wait", true, None).state_key(), "wait");
    }

    #[test]
    fn test_decision_times_format_as_utc_instants() {
        let decision = decision("wait", true, Some(60));
        assert_eq!(decision.evaluated_at(), "1970-01-01T00:00:00Z");
        assert_eq!(decision.next_eligible_at(), "1970-01-01T00:01:00Z");
    }

    #[test]
    fn test_next_eligible_falls_back_to_a_dash_without_a_retry_window() {
        assert_eq!(decision("deny", true, None).next_eligible_at(), "-");
    }

    #[test]
    fn test_filters_build_a_query_from_the_trimmed_names() {
        let filters = super::ShadowInspectionFilters {
            repository: "  root/alpha ".to_owned(),
            project: " flask ".to_owned(),
            limit: "50".to_owned(),
        };
        assert_eq!(
            filters.url(Some("flask-1.0.bin\u{1f}0\u{1f}hosted")).unwrap(),
            "/+shadow/candidates?repository=root%2Falpha&project=flask&limit=50&cursor=flask-1.0.bin%1F0%1Fhosted",
        );
    }

    #[rstest]
    #[case::no_repository("", "flask")]
    #[case::no_project("root/alpha", "")]
    fn test_filters_reject_a_blank_name(#[case] repository: &str, #[case] project: &str) {
        let filters = super::ShadowInspectionFilters {
            repository: repository.to_owned(),
            project: project.to_owned(),
            limit: "25".to_owned(),
        };
        assert_eq!(
            filters.url(None),
            Err("Enter a repository and a project to inspect.".to_owned())
        );
    }
}
