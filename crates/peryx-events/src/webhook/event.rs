use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEnvelope {
    pub schema: &'static str,
    pub event: &'static str,
    pub data: Value,
}

impl WebhookEnvelope {
    /// # Panics
    /// Panics when `schema` or `event` is not a stable lowercase identifier.
    #[must_use]
    pub fn new(schema: &'static str, event: &'static str, data: Value) -> Self {
        assert!(valid_identifier(schema), "invalid webhook schema");
        assert!(valid_identifier(event), "invalid webhook event");
        Self { schema, event, data }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEvent {
    pub created_at_unix: i64,
    pub index: String,
    pub envelope: WebhookEnvelope,
}

pub(super) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.'))
}

#[cfg(test)]
#[path = "../../tests/unit/webhook/event/tests.rs"]
mod tests;
