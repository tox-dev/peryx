use super::*;
use rstest::rstest;

#[test]
fn test_envelope_preserves_opaque_data() {
    let data = serde_json::json!({
        "subject": {"key": "value"},
        "items": [1, 2],
    });

    assert_eq!(
        WebhookEnvelope::new("owner.v1", "resource-write", data.clone()),
        WebhookEnvelope {
            schema: "owner.v1",
            event: "resource-write",
            data,
        }
    );
}

#[rstest]
#[case::versioned("owner.v1", true)]
#[case::hyphenated("resource-write", true)]
#[case::numbered("event2", true)]
#[case::empty("", false)]
#[case::uppercase("Invalid", false)]
#[case::underscore("invalid_name", false)]
#[case::slash("invalid/name", false)]
fn test_identifier_validation(#[case] value: &str, #[case] expected: bool) {
    assert_eq!(valid_identifier(value), expected);
}

#[test]
#[should_panic(expected = "invalid webhook schema")]
fn test_envelope_rejects_invalid_schema() {
    drop(WebhookEnvelope::new("Owner", "resource-write", serde_json::Value::Null));
}

#[test]
#[should_panic(expected = "invalid webhook event")]
fn test_envelope_rejects_invalid_event() {
    drop(WebhookEnvelope::new(
        "owner.v1",
        "resource_write",
        serde_json::Value::Null,
    ));
}
