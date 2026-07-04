//! Structured security-relevant index events.

use axum::http::{HeaderMap, header};

const UNKNOWN: &str = "unknown";

pub(crate) struct Event<'a> {
    action: &'static str,
    result: &'static str,
    actor: Option<&'a str>,
    index: Option<&'a str>,
    source_index: Option<&'a str>,
    local_index: Option<&'a str>,
    project: Option<&'a str>,
    version: Option<&'a str>,
    filename: Option<&'a str>,
    digest: Option<&'a str>,
    count: usize,
    changed: bool,
    reason: Option<&'a str>,
    request_id: Option<&'a str>,
    user_agent: Option<&'a str>,
}

impl<'a> Event<'a> {
    pub(crate) const fn new(action: &'static str, result: &'static str) -> Self {
        Self {
            action,
            result,
            actor: None,
            index: None,
            source_index: None,
            local_index: None,
            project: None,
            version: None,
            filename: None,
            digest: None,
            count: 0,
            changed: false,
            reason: None,
            request_id: None,
            user_agent: None,
        }
    }

    pub(crate) const fn actor(mut self, actor: Option<&'a str>) -> Self {
        self.actor = actor;
        self
    }

    pub(crate) const fn index(mut self, index: &'a str) -> Self {
        self.index = Some(index);
        self
    }

    pub(crate) const fn source_index(mut self, source_index: &'a str) -> Self {
        self.source_index = Some(source_index);
        self
    }

    pub(crate) const fn local_index(mut self, local_index: &'a str) -> Self {
        self.local_index = Some(local_index);
        self
    }

    pub(crate) const fn project(mut self, project: Option<&'a str>) -> Self {
        self.project = project;
        self
    }

    pub(crate) const fn version(mut self, version: Option<&'a str>) -> Self {
        self.version = version;
        self
    }

    pub(crate) const fn filename(mut self, filename: Option<&'a str>) -> Self {
        self.filename = filename;
        self
    }

    pub(crate) const fn digest(mut self, digest: Option<&'a str>) -> Self {
        self.digest = digest;
        self
    }

    pub(crate) const fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    pub(crate) const fn changed(mut self, changed: bool) -> Self {
        self.changed = changed;
        self
    }

    pub(crate) const fn reason(mut self, reason: Option<&'a str>) -> Self {
        self.reason = reason;
        self
    }

    pub(crate) fn request(mut self, headers: &'a HeaderMap) -> Self {
        self.request_id = request_id(headers);
        self.user_agent = user_agent(headers);
        self
    }

    pub(crate) fn emit(&self) {
        let actor = text(self.actor);
        let index = text(self.index);
        let source_index = text(self.source_index);
        let local_index = text(self.local_index);
        let project = text(self.project);
        let version = text(self.version);
        let filename = text(self.filename);
        let digest = text(self.digest);
        let reason = text(self.reason);
        let request_id = text(self.request_id);
        let user_agent = text(self.user_agent);
        tracing::info!(
            target: "velodex::security",
            security_event = true,
            event = "index_action",
            action = self.action,
            result = self.result,
            actor,
            index,
            source_index,
            local_index,
            project,
            version,
            filename,
            digest,
            count = self.count,
            changed = self.changed,
            reason,
            request_id,
            user_agent,
            "index security event"
        );
    }
}

#[must_use]
pub(crate) fn actor(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())?;
    let credentials = velodex_identity::parse_basic(value)?;
    Some(if credentials.user.is_empty() {
        UNKNOWN.to_owned()
    } else {
        credentials.user
    })
}

fn request_id(headers: &HeaderMap) -> Option<&str> {
    header_str(headers, "x-request-id")
}

fn user_agent(headers: &HeaderMap) -> Option<&str> {
    header_str(headers, header::USER_AGENT.as_str())
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn text(value: Option<&str>) -> &str {
    value.unwrap_or("")
}
