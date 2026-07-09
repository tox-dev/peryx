//! Webhook target configuration and the resolved delivery runtime.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;

use url::Url;

use super::event::WebhookEventKind;

pub struct WebhookRuntime {
    pub(super) client: reqwest::Client,
    targets: HashMap<String, Vec<WebhookTarget>>,
    pub(super) running: AtomicBool,
    pub(super) notify: tokio::sync::Notify,
}

impl WebhookRuntime {
    /// Runtime with no configured targets.
    #[must_use]
    pub fn disabled() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Self {
            client: reqwest::Client::new(),
            targets: HashMap::new(),
            running: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Build a runtime from resolved configuration.
    ///
    /// # Errors
    /// Returns an error for duplicate target names, invalid URLs, empty secrets, or unknown events.
    pub fn new(configs: Vec<WebhookTargetConfig>) -> Result<Self, WebhookConfigError> {
        let mut seen = HashSet::new();
        let mut targets: HashMap<String, Vec<WebhookTarget>> = HashMap::new();
        for config in configs {
            if config.name.is_empty() {
                return Err(WebhookConfigError::EmptyName { index: config.index });
            }
            if config.secret.is_empty() {
                return Err(WebhookConfigError::EmptySecret {
                    index: config.index,
                    target: config.name,
                });
            }
            if !seen.insert((config.index.clone(), config.name.clone())) {
                return Err(WebhookConfigError::Duplicate {
                    index: config.index,
                    target: config.name,
                });
            }
            targets.entry(config.index).or_default().push(WebhookTarget {
                name: config.name,
                url: target_url(&config.url)?,
                secret: config.secret,
                events: WebhookEvents::new(config.events)?,
            });
        }
        Ok(Self {
            targets,
            ..Self::disabled()
        })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub(super) fn target_names(&self, index: &str, event: WebhookEventKind) -> Vec<String> {
        self.targets.get(index).map_or_else(Vec::new, |targets| {
            targets
                .iter()
                .filter(|target| target.events.matches(event))
                .map(|target| target.name.clone())
                .collect()
        })
    }

    pub(super) fn target(&self, index: &str, name: &str) -> Option<WebhookTarget> {
        self.targets
            .get(index)?
            .iter()
            .find(|target| target.name == name)
            .cloned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookTargetConfig {
    pub index: String,
    pub name: String,
    pub url: String,
    pub secret: String,
    pub events: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WebhookConfigError {
    #[error("webhook target name is empty on index {index}")]
    EmptyName { index: String },
    #[error("webhook target {target} on index {index} has an empty secret")]
    EmptySecret { index: String, target: String },
    #[error("duplicate webhook target {target} on index {index}")]
    Duplicate { index: String, target: String },
    #[error("webhook target URL {url:?} is invalid: {source}")]
    InvalidUrl { url: String, source: url::ParseError },
    #[error("webhook target URL {url:?} must use http or https")]
    InvalidScheme { url: String },
    #[error("webhook target URL {url:?} must not include credentials, query, or fragment")]
    SensitiveUrlParts { url: String },
    #[error("unknown webhook event {0:?}")]
    UnknownEvent(String),
}

#[derive(Debug, Clone)]
pub(super) struct WebhookTarget {
    name: String,
    pub(super) url: Url,
    pub(super) secret: String,
    events: WebhookEvents,
}

#[derive(Debug, Clone)]
struct WebhookEvents {
    all: bool,
    events: HashSet<WebhookEventKind>,
}

impl WebhookEvents {
    fn new(names: Vec<String>) -> Result<Self, WebhookConfigError> {
        if names.is_empty() {
            return Ok(Self {
                all: true,
                events: HashSet::new(),
            });
        }
        Ok(Self {
            all: false,
            events: names
                .into_iter()
                .map(|name| WebhookEventKind::parse(&name).ok_or(WebhookConfigError::UnknownEvent(name)))
                .collect::<Result<_, _>>()?,
        })
    }

    fn matches(&self, event: WebhookEventKind) -> bool {
        self.all || self.events.contains(&event)
    }
}

fn target_url(raw: &str) -> Result<Url, WebhookConfigError> {
    let url = Url::parse(raw).map_err(|source| WebhookConfigError::InvalidUrl {
        url: raw.to_owned(),
        source,
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(WebhookConfigError::InvalidScheme { url: raw.to_owned() });
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err(WebhookConfigError::SensitiveUrlParts { url: raw.to_owned() });
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    type ErrorMatch = fn(&WebhookConfigError) -> bool;

    fn target(name: &str, url: &str, secret: &str, events: &[&str]) -> WebhookTargetConfig {
        WebhookTargetConfig {
            index: "hosted".to_owned(),
            name: name.to_owned(),
            url: url.to_owned(),
            secret: secret.to_owned(),
            events: events.iter().map(|&event| event.to_owned()).collect(),
        }
    }

    #[test]
    fn test_runtime_matches_all_events_when_no_filter_is_set() {
        let runtime = WebhookRuntime::new(vec![target("ci", "https://ci.example/hook", "secret", &[])]).unwrap();

        assert_eq!(runtime.target_names("hosted", WebhookEventKind::Upload), ["ci"]);
        assert_eq!(runtime.target_names("hosted", WebhookEventKind::Management), ["ci"]);
        assert!(runtime.target_names("other", WebhookEventKind::Upload).is_empty());
    }

    #[rstest]
    #[case::empty_name(vec![target("", "https://ci.example/hook", "secret", &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::EmptyName { .. }))]
    #[case::empty_secret(vec![target("ci", "https://ci.example/hook", "", &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::EmptySecret { .. }))]
    #[case::duplicate(
        vec![
            target("ci", "https://ci.example/hook", "secret", &[]),
            target("ci", "https://ci.example/other", "secret", &[]),
        ],
        |err: &WebhookConfigError| matches!(err, WebhookConfigError::Duplicate { .. })
    )]
    #[case::invalid_url(vec![target("ci", "not a url", "secret", &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::InvalidUrl { .. }))]
    #[case::sensitive_url_parts(vec![target("ci", "https://ci.example/hook?token=secret", "secret", &[])], |err: &WebhookConfigError| matches!(err, WebhookConfigError::SensitiveUrlParts { .. }))]
    fn test_runtime_rejects_invalid_target_config(
        #[case] configs: Vec<WebhookTargetConfig>,
        #[case] matches_error: ErrorMatch,
    ) {
        let Err(err) = WebhookRuntime::new(configs) else {
            panic!("expected an invalid-config error");
        };
        assert!(matches_error(&err));
    }

    #[test]
    fn test_runtime_rejects_unknown_event() {
        assert!(matches!(
            WebhookRuntime::new(vec![target("ci", "https://ci.example/hook", "secret", &["bogus"])]),
            Err(WebhookConfigError::UnknownEvent(event)) if event == "bogus"
        ));
    }
}
