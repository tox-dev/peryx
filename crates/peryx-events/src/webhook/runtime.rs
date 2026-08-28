use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

use super::event::valid_identifier;

const MIN_SECRET_BYTES: usize = 32;

pub struct WebhookRuntime {
    pub(super) client: reqwest::Client,
    targets: HashMap<String, Vec<WebhookTarget>>,
    pub(super) running: Arc<AtomicBool>,
    pub(super) stopped: tokio::sync::watch::Sender<()>,
    pub(super) notify: tokio::sync::Notify,
}

impl WebhookRuntime {
    #[must_use]
    pub fn disabled() -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let (stopped, _) = tokio::sync::watch::channel(());
        Self {
            client: delivery_client(),
            targets: HashMap::new(),
            running: Arc::new(AtomicBool::new(false)),
            stopped,
            notify: tokio::sync::Notify::new(),
        }
    }

    /// # Errors
    /// Returns an error for duplicate target names, invalid URLs, undersized secrets, or invalid event names.
    pub fn new(configs: Vec<WebhookTargetConfig>) -> Result<Self, WebhookConfigError> {
        let mut seen = HashSet::new();
        let mut targets: HashMap<String, Vec<WebhookTarget>> = HashMap::new();
        for config in configs {
            if config.name.is_empty() {
                return Err(WebhookConfigError::EmptyName { index: config.index });
            }
            if config.secret.len() < MIN_SECRET_BYTES {
                return Err(WebhookConfigError::SecretTooShort {
                    index: config.index,
                    target: config.name,
                    minimum: MIN_SECRET_BYTES,
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

    pub async fn wait_until_idle(&self) {
        let mut stopped = self.stopped.subscribe();
        while self.running.load(Ordering::Acquire) {
            stopped.changed().await.unwrap_or(());
        }
    }

    pub(super) fn target_names(&self, index: &str, event: &str) -> Vec<String> {
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
    #[error("webhook target {target} on index {index} secret must contain at least {minimum} bytes")]
    SecretTooShort {
        index: String,
        target: String,
        minimum: usize,
    },
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
    events: HashSet<String>,
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
                .map(|name| {
                    valid_identifier(&name)
                        .then_some(name.clone())
                        .ok_or(WebhookConfigError::UnknownEvent(name))
                })
                .collect::<Result<_, _>>()?,
        })
    }

    fn matches(&self, event: &str) -> bool {
        self.all || self.events.contains(event)
    }
}

fn delivery_client() -> reqwest::Client {
    // Redirects could expose the signed payload outside the configured origin.
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("webhook delivery client builds")
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
#[path = "../../tests/unit/webhook/runtime/tests.rs"]
mod tests;
