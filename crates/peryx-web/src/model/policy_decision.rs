use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, PrimitiveDateTime};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiPolicyDecisionPage {
    pub decisions: Vec<UiPolicyDecision>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct UiPolicyDecision {
    pub id: String,
    pub repository: String,
    pub resource: String,
    pub group: Option<String>,
    pub artifact: Option<String>,
    pub source: Option<String>,
    pub action: String,
    pub state: String,
    pub rule: Option<String>,
    pub reason: Option<String>,
    pub evaluated_at_unix: i64,
    pub next_eligible_at_unix: Option<i64>,
    pub fresh: bool,
}

impl UiPolicyDecision {
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

    #[must_use]
    pub fn evaluated_at(&self) -> String {
        format_unix(self.evaluated_at_unix)
    }

    #[must_use]
    pub fn next_eligible_at(&self) -> String {
        self.next_eligible_at_unix.map_or_else(|| "-".to_owned(), format_unix)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyDecisionFilters {
    pub repository: String,
    pub state: String,
    pub rule: String,
    pub source: String,
    pub from: String,
    pub to: String,
    pub limit: String,
}

impl Default for PolicyDecisionFilters {
    fn default() -> Self {
        Self {
            repository: String::new(),
            state: String::new(),
            rule: String::new(),
            source: String::new(),
            from: String::new(),
            to: String::new(),
            limit: "25".to_owned(),
        }
    }
}

impl PolicyDecisionFilters {
    /// Build the API URL only after validating browser-local date inputs.
    ///
    /// # Errors
    /// Returns a bounded message when either UTC date input is invalid.
    pub fn url(&self, cursor: Option<&str>) -> Result<String, String> {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in [
            ("repository", self.repository.trim()),
            ("state", self.state.trim()),
            ("rule", self.rule.trim()),
            ("source", self.source.trim()),
        ] {
            if !value.is_empty() {
                serializer.append_pair(name, value);
            }
        }
        for (name, value) in [("from", self.from.trim()), ("to", self.to.trim())] {
            if !value.is_empty() {
                serializer.append_pair(name, &parse_datetime(value)?.to_string());
            }
        }
        serializer.append_pair("limit", self.limit.trim());
        if let Some(cursor) = cursor {
            serializer.append_pair("cursor", cursor);
        }
        Ok(format!("/+policy/decisions?{}", serializer.finish()))
    }
}

fn parse_datetime(value: &str) -> Result<i64, String> {
    let format = time::format_description::parse_borrowed::<3>("[year]-[month]-[day]T[hour]:[minute]")
        .expect("the datetime-local format is valid");
    PrimitiveDateTime::parse(value, &format)
        .map(PrimitiveDateTime::assume_utc)
        .map(OffsetDateTime::unix_timestamp)
        .map_err(|_| format!("Invalid UTC date and time: {value}"))
}

fn format_unix(value: i64) -> String {
    let Ok(time) = OffsetDateTime::from_unix_timestamp(value) else {
        return value.to_string();
    };
    time.format(&Rfc3339).unwrap_or_else(|_| value.to_string())
}
