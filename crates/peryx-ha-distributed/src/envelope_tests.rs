use serde_json::json;

use crate::{
    AuthorityEpoch, BlobReference, Change, DEFAULT_DECODE_LIMITS, DecodeLimits, EnvelopeError, MetadataMutation,
    OperationEnvelope, OperationKind, SCHEMA_VERSION, SchemaVersion, TraceContext, TraceError, derive_child,
};

const VALID_TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn change() -> Change {
    Change {
        serial: 7,
        event: b"upload-event-payload".to_vec(),
        metadata: vec![MetadataMutation::Put {
            key: "pypi/simple/example".to_owned(),
            value: b"secret-digest-map".to_vec(),
        }],
        blobs: vec![BlobReference {
            sha256: "a".repeat(64),
            size: 1024,
        }],
    }
}

fn envelope() -> OperationEnvelope {
    OperationEnvelope::current("primary-a", AuthorityEpoch(3), OperationKind::Publish, change())
}

fn traced(traceparent: &str) -> OperationEnvelope {
    OperationEnvelope {
        trace: Some(TraceContext {
            traceparent: traceparent.to_owned(),
            tracestate: None,
        }),
        ..envelope()
    }
}

#[test]
fn test_envelope_round_trips_through_encode_decode() {
    let original = envelope();
    let decoded = OperationEnvelope::decode(&original.encode(), DecodeLimits::default()).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn test_visibility_kind_round_trips_as_its_kebab_case_tag() {
    let original = OperationEnvelope::current("primary-a", AuthorityEpoch(3), OperationKind::Visibility, change());
    let bytes = original.encode();

    assert!(
        String::from_utf8_lossy(&bytes).contains("\"visibility\""),
        "the kind serializes to its kebab-case tag"
    );
    let decoded = OperationEnvelope::decode(&bytes, DecodeLimits::default()).unwrap();
    assert_eq!(decoded.kind, OperationKind::Visibility);
}

#[test]
fn test_envelope_current_sets_schema_version_and_no_trace() {
    let envelope = envelope();
    assert_eq!(envelope.schema_version, SCHEMA_VERSION);
    assert_eq!(envelope.trace, None);
    assert_eq!(envelope.change, change());
}

#[test]
fn test_envelope_identity_is_source_epoch_serial() {
    let envelope = envelope();
    let identity = envelope.identity();
    assert_eq!(identity.source, "primary-a");
    assert_eq!(identity.epoch, AuthorityEpoch(3));
    assert_eq!(identity.serial, 7);
    assert_eq!(identity.to_string(), "primary-a@3#7");
}

#[test]
fn test_envelope_display_shows_kind_version_identity_only() {
    assert_eq!(envelope().to_string(), "publish v1 primary-a@3#7");
}

#[test]
fn test_envelope_debug_shows_identity_but_omits_payload() {
    let rendered = format!("{:?}", traced(VALID_TRACEPARENT));
    assert!(rendered.contains("primary-a"), "{rendered}");
    assert!(rendered.contains("serial: 7"), "{rendered}");
    assert!(rendered.contains(VALID_TRACEPARENT), "{rendered}");
    assert!(rendered.contains(".."), "{rendered}");
    assert!(!rendered.contains("secret-digest-map"), "{rendered}");
    assert!(!rendered.contains("upload-event-payload"), "{rendered}");
}

#[test]
fn test_envelope_display_omits_payload() {
    let rendered = envelope().to_string();
    assert!(!rendered.contains("secret-digest-map"), "{rendered}");
    assert!(!rendered.contains("upload-event-payload"), "{rendered}");
}

#[test]
fn test_envelope_decode_rejects_oversized() {
    let bytes = envelope().encode();
    let limits = DecodeLimits {
        max_bytes: bytes.len() - 1,
        ..DecodeLimits::default()
    };
    let error = OperationEnvelope::decode(&bytes, limits).unwrap_err();
    assert!(matches!(error, EnvelopeError::TooLarge { limit, actual }
        if limit == bytes.len() - 1 && actual == bytes.len()));
    assert!(error.to_string().contains("decode limit"));
}

#[test]
fn test_envelope_decode_rejects_too_deep() {
    let limits = DecodeLimits {
        max_depth: 1,
        ..DecodeLimits::default()
    };
    let error = OperationEnvelope::decode(&envelope().encode(), limits).unwrap_err();
    assert!(matches!(error, EnvelopeError::TooDeep { limit: 1 }));
    assert!(error.to_string().contains("nests past"));
}

#[test]
fn test_envelope_decode_rejects_malformed_json() {
    let error = OperationEnvelope::decode(b"not json", DecodeLimits::default()).unwrap_err();
    assert!(matches!(error, EnvelopeError::Malformed(_)));
    assert!(error.to_string().contains("malformed"));
}

#[test]
fn test_envelope_decode_rejects_empty_source() {
    let bytes = OperationEnvelope::current("", AuthorityEpoch(1), OperationKind::Withdraw, change()).encode();
    let error = OperationEnvelope::decode(&bytes, DecodeLimits::default()).unwrap_err();
    assert!(matches!(error, EnvelopeError::EmptySource));
    assert!(error.to_string().contains("empty source"));
}

#[test]
fn test_envelope_decode_rejects_any_version_but_the_current_one() {
    for offending in [SchemaVersion(0), SchemaVersion(2)] {
        let bytes = OperationEnvelope {
            schema_version: offending,
            ..envelope()
        }
        .encode();
        let error = OperationEnvelope::decode(&bytes, DecodeLimits::default()).unwrap_err();
        assert!(
            matches!(error, EnvelopeError::UnsupportedVersion { version, expected }
                if version == offending && expected == SCHEMA_VERSION),
            "{offending}: {error}"
        );
        assert!(
            error.to_string().contains(&format!(
                "unsupported envelope schema version {offending}; this build accepts v1"
            )),
            "{offending}: {error}"
        );
    }
}

#[test]
fn test_envelope_decode_ignores_unknown_fields() {
    let mut value: serde_json::Value = serde_json::from_slice(&envelope().encode()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("field_from_a_newer_schema".to_owned(), json!({"nested": [1, 2, 3]}));
    let bytes = serde_json::to_vec(&value).unwrap();
    let decoded = OperationEnvelope::decode(&bytes, DecodeLimits::default()).unwrap();
    assert_eq!(decoded, envelope());
}

#[test]
fn test_envelope_decode_accepts_valid_traceparent_and_tracestate() {
    let original = OperationEnvelope {
        trace: Some(TraceContext {
            traceparent: VALID_TRACEPARENT.to_owned(),
            tracestate: Some("vendor=value".to_owned()),
        }),
        ..envelope()
    };
    let decoded = OperationEnvelope::decode(&original.encode(), DecodeLimits::default()).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn test_envelope_decode_walks_string_escapes_without_counting_brackets() {
    let original = OperationEnvelope {
        source: "src\"with{}[]\\escapes".to_owned(),
        ..envelope()
    };
    let decoded = OperationEnvelope::decode(&original.encode(), DecodeLimits::default()).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn test_envelope_decode_accepts_an_unrecognized_non_ff_version() {
    let traceparent = "fe-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let original = traced(traceparent);
    let decoded = OperationEnvelope::decode(&original.encode(), DecodeLimits::default()).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn test_envelope_decode_accepts_a_later_version_with_an_extension() {
    let traceparent = "fe-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-what-comes-next";
    let original = traced(traceparent);
    let decoded = OperationEnvelope::decode(&original.encode(), DecodeLimits::default()).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn test_envelope_decode_rejects_each_malformed_traceparent() {
    let cases = [
        "too-few-parts",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        "fe-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-",
        "0-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1",
        "0g-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        "0A-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00F067AA0BA902B7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0A",
        "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        "FF-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        "fF-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
    ];
    for traceparent in cases {
        let error = OperationEnvelope::decode(&traced(traceparent).encode(), DecodeLimits::default()).unwrap_err();
        assert!(
            matches!(&error, EnvelopeError::InvalidTrace(reported) if reported == traceparent),
            "{traceparent}: {error}"
        );
        assert!(error.to_string().contains("traceparent"), "{traceparent}");
    }
}

#[test]
fn test_schema_version_displays_with_v_prefix() {
    assert_eq!(SchemaVersion(1).to_string(), "v1");
}

#[test]
fn test_operation_kind_as_str_and_display_match() {
    let cases = [
        (OperationKind::Publish, "publish"),
        (OperationKind::Withdraw, "withdraw"),
        (OperationKind::Delete, "delete"),
        (OperationKind::CacheFill, "cache-fill"),
        (OperationKind::Publish, "publish"),
        (OperationKind::Delete, "delete"),
        (OperationKind::Visibility, "visibility"),
    ];
    for (kind, expected) in cases {
        assert_eq!(kind.as_str(), expected);
        assert_eq!(kind.to_string(), expected);
    }
}

#[test]
fn test_decode_limits_default_is_the_shared_constant() {
    assert_eq!(DecodeLimits::default(), DEFAULT_DECODE_LIMITS);
    assert_eq!(DEFAULT_DECODE_LIMITS.max_bytes, 1 << 20);
    assert_eq!(DEFAULT_DECODE_LIMITS.max_depth, 32);
}

const CHILD_SPAN: &str = "b7ad6b7169203331";

#[test]
fn test_derive_child_continues_the_parent_trace() {
    let child = derive_child(VALID_TRACEPARENT, CHILD_SPAN).unwrap();

    assert_eq!(child, "00-4bf92f3577b34da6a3ce929d0e0e4736-b7ad6b7169203331-01");
}

#[test]
fn test_derive_child_preserves_the_parent_flags() {
    let parent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";

    let child = derive_child(parent, CHILD_SPAN).unwrap();

    assert_eq!(child, "00-4bf92f3577b34da6a3ce929d0e0e4736-b7ad6b7169203331-00");
}

#[test]
fn test_derive_child_rejects_a_malformed_parent() {
    let parent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa-01";

    let error = derive_child(parent, CHILD_SPAN).unwrap_err();

    assert_eq!(error, TraceError::MalformedParent(parent.to_owned()));
}

#[test]
fn test_derive_child_rejects_a_reserved_ff_version_parent() {
    let parent = "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    let error = derive_child(parent, CHILD_SPAN).unwrap_err();

    assert_eq!(error, TraceError::MalformedParent(parent.to_owned()));
}

#[test]
fn test_derive_child_rejects_an_invalid_span_id() {
    for span in ["short", "zzzzzzzzzzzzzzzz", "0000000000000000"] {
        let error = derive_child(VALID_TRACEPARENT, span).unwrap_err();
        assert_eq!(error, TraceError::InvalidSpanId(span.to_owned()), "{span}");
    }
}
