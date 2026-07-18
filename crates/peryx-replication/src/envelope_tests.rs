use serde_json::json;

use crate::{
    AuthorityEpoch, BlobReference, CURRENT_SCHEMA_VERSION, Change, DEFAULT_DECODE_LIMITS, DecodeLimits, EnvelopeError,
    MetadataMutation, OperationEnvelope, OperationKind, SchemaVersion, TraceContext,
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
    OperationEnvelope::current("primary-a", AuthorityEpoch(3), OperationKind::Upload, change())
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
fn test_envelope_current_sets_schema_version_and_no_trace() {
    let envelope = envelope();
    assert_eq!(envelope.schema_version, CURRENT_SCHEMA_VERSION);
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
    assert_eq!(envelope().to_string(), "upload v1 primary-a@3#7");
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
    let bytes = OperationEnvelope::current("", AuthorityEpoch(1), OperationKind::Yank, change()).encode();
    let error = OperationEnvelope::decode(&bytes, DecodeLimits::default()).unwrap_err();
    assert!(matches!(error, EnvelopeError::EmptySource));
    assert!(error.to_string().contains("empty source"));
}

#[test]
fn test_envelope_decode_rejects_version_below_window() {
    let bytes = OperationEnvelope {
        schema_version: SchemaVersion(0),
        ..envelope()
    }
    .encode();
    let error = OperationEnvelope::decode(&bytes, DecodeLimits::default()).unwrap_err();
    assert!(matches!(error, EnvelopeError::UnsupportedVersion { version, min, max }
        if version == SchemaVersion(0) && min == SchemaVersion(1) && max == SchemaVersion(1)));
    assert!(error.to_string().contains("unsupported envelope schema version v0"));
}

#[test]
fn test_envelope_decode_rejects_version_above_window() {
    let bytes = OperationEnvelope {
        schema_version: SchemaVersion(2),
        ..envelope()
    }
    .encode();
    let error = OperationEnvelope::decode(&bytes, DecodeLimits::default()).unwrap_err();
    assert!(matches!(error, EnvelopeError::UnsupportedVersion { version, .. } if version == SchemaVersion(2)));
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
fn test_envelope_decode_rejects_each_malformed_traceparent() {
    let cases = [
        "too-few-parts",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        "0-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1",
        "0g-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
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
fn test_schema_version_negotiate_picks_highest_common() {
    assert_eq!(
        SchemaVersion::negotiate(SchemaVersion(1)..=SchemaVersion(3), SchemaVersion(2)..=SchemaVersion(5)),
        Some(SchemaVersion(3)),
    );
    assert_eq!(
        SchemaVersion::negotiate(SchemaVersion(1)..=SchemaVersion(2), SchemaVersion(2)..=SchemaVersion(4)),
        Some(SchemaVersion(2)),
    );
}

#[test]
fn test_schema_version_negotiate_returns_none_when_disjoint() {
    assert_eq!(
        SchemaVersion::negotiate(SchemaVersion(1)..=SchemaVersion(1), SchemaVersion(3)..=SchemaVersion(4)),
        None,
    );
}

#[test]
fn test_operation_kind_as_str_and_display_match() {
    let cases = [
        (OperationKind::Upload, "upload"),
        (OperationKind::Yank, "yank"),
        (OperationKind::Delete, "delete"),
        (OperationKind::CacheFill, "cache-fill"),
        (OperationKind::OciPush, "oci-push"),
        (OperationKind::OciDelete, "oci-delete"),
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
