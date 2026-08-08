use super::{
    UiArtifactSource, UiAttestation, UiByteAvailability, UiFile, UiOperationStatus, UiProvenance, UiProvenanceSource,
    UiSubjectMatch,
};

#[test]
fn test_provenance_source_and_subject_match_round_trip_snake_case() {
    for (source, wire) in [
        (UiProvenanceSource::Hosted, "\"hosted\""),
        (UiProvenanceSource::Mirrored, "\"mirrored\""),
    ] {
        assert_eq!(serde_json::to_string(&source).unwrap(), wire);
        assert_eq!(serde_json::from_str::<UiProvenanceSource>(wire).unwrap(), source);
        assert_eq!(format!("\"{}\"", source.as_str()), wire);
    }
    for (subject, wire) in [
        (UiSubjectMatch::Matched, "\"matched\""),
        (UiSubjectMatch::Mismatched, "\"mismatched\""),
        (UiSubjectMatch::Unknown, "\"unknown\""),
    ] {
        assert_eq!(serde_json::to_string(&subject).unwrap(), wire);
        assert_eq!(serde_json::from_str::<UiSubjectMatch>(wire).unwrap(), subject);
        assert_eq!(format!("\"{}\"", subject.as_str()), wire);
    }
}

#[test]
fn test_provenance_omits_empty_attestations_and_absent_malformed() {
    let provenance = UiProvenance {
        source: UiProvenanceSource::Mirrored,
        attestations: Vec::new(),
        malformed: false,
    };
    let json = serde_json::to_string(&provenance).unwrap();
    assert_eq!(json, r#"{"source":"mirrored"}"#);
    assert_eq!(serde_json::from_str::<UiProvenance>(&json).unwrap(), provenance);
}

#[test]
fn test_provenance_carries_attestations_and_malformed_on_the_wire() {
    let provenance = UiProvenance {
        source: UiProvenanceSource::Hosted,
        attestations: vec![UiAttestation {
            predicate_type: Some("https://docs.alpha.org/attestations/publish/v1".to_owned()),
            subject: UiSubjectMatch::Matched,
        }],
        malformed: true,
    };
    let json = serde_json::to_string(&provenance).unwrap();
    assert!(json.contains(r#""source":"hosted""#), "{json}");
    assert!(json.contains(r#""subject":"matched""#), "{json}");
    assert!(json.contains(r#""malformed":true"#), "{json}");
    assert_eq!(serde_json::from_str::<UiProvenance>(&json).unwrap(), provenance);
}

#[test]
fn test_attestation_omits_an_absent_predicate_type() {
    let attestation = UiAttestation {
        predicate_type: None,
        subject: UiSubjectMatch::Unknown,
    };
    let json = serde_json::to_string(&attestation).unwrap();
    assert!(!json.contains("predicate_type"), "{json}");
    assert_eq!(serde_json::from_str::<UiAttestation>(&json).unwrap(), attestation);
}

#[test]
fn test_source_and_availability_round_trip_snake_case() {
    for (source, wire) in [
        (UiArtifactSource::Hosted, "\"hosted\""),
        (UiArtifactSource::Proxy, "\"proxy\""),
        (UiArtifactSource::Generated, "\"generated\""),
    ] {
        assert_eq!(serde_json::to_string(&source).unwrap(), wire);
        assert_eq!(serde_json::from_str::<UiArtifactSource>(wire).unwrap(), source);
        assert_eq!(format!("\"{}\"", source.as_str()), wire);
    }
    for (availability, wire) in [
        (UiByteAvailability::Local, "\"local\""),
        (UiByteAvailability::RemoteOnly, "\"remote_only\""),
        (UiByteAvailability::Unavailable, "\"unavailable\""),
    ] {
        assert_eq!(serde_json::to_string(&availability).unwrap(), wire);
        assert_eq!(serde_json::from_str::<UiByteAvailability>(wire).unwrap(), availability);
        assert_eq!(format!("\"{}\"", availability.as_str()), wire);
    }
}

#[test]
fn test_operation_status_round_trips_snake_case() {
    for (status, wire) in [
        (UiOperationStatus::Pending, "\"pending\""),
        (UiOperationStatus::Published, "\"published\""),
        (UiOperationStatus::Failed, "\"failed\""),
        (UiOperationStatus::Expired, "\"expired\""),
    ] {
        assert_eq!(serde_json::to_string(&status).unwrap(), wire);
        assert_eq!(serde_json::from_str::<UiOperationStatus>(wire).unwrap(), status);
        assert_eq!(format!("\"{}\"", status.as_str()), wire);
    }
}

#[test]
fn test_operation_status_derives_from_the_durable_fields() {
    // Terminal states are independent of the clock; a pending write reads expired only once the clock
    // reaches its retention deadline.
    assert_eq!(
        UiOperationStatus::derive(true, false, Some(10), 5),
        UiOperationStatus::Published
    );
    assert_eq!(
        UiOperationStatus::derive(false, true, None, 5),
        UiOperationStatus::Failed
    );
    assert_eq!(
        UiOperationStatus::derive(false, false, Some(10), 10),
        UiOperationStatus::Expired
    );
    assert_eq!(
        UiOperationStatus::derive(false, false, Some(10), 9),
        UiOperationStatus::Pending
    );
    assert_eq!(
        UiOperationStatus::derive(false, false, None, 9),
        UiOperationStatus::Pending
    );
}

#[test]
fn test_ui_file_carries_source_and_availability_on_the_wire() {
    let file = UiFile {
        filename: "pkg-1.0-py3-none-any.bin".to_owned(),
        release: Some("1.0".to_owned()),
        url: "/alpha/files/aa/pkg-1.0-py3-none-any.bin".to_owned(),
        sha256: "aa".to_owned(),
        size: Some(10),
        upload_time: None,
        yanked: false,
        yanked_reason: None,
        has_metadata: false,
        upstream: Some("mirror".to_owned()),
        provenance: Some("https://alpha.example/files/aa/pkg-1.0-py3-none-any.bin.provenance".to_owned()),
        provenance_detail: Some(UiProvenance {
            source: UiProvenanceSource::Mirrored,
            attestations: Vec::new(),
            malformed: false,
        }),
        source: UiArtifactSource::Proxy,
        availability: UiByteAvailability::RemoteOnly,
        browsable: true,
    };
    let json = serde_json::to_string(&file).unwrap();
    assert!(json.contains("\"source\":\"proxy\""), "{json}");
    assert!(json.contains("\"availability\":\"remote_only\""), "{json}");
    assert!(json.contains("\"browsable\":true"), "{json}");
    assert!(json.contains("\"upstream\":\"mirror\""), "{json}");
    assert!(json.contains("\"release\":\"1.0\""), "{json}");
    assert!(
        json.contains("\"provenance\":\"https://alpha.example/files/aa/pkg-1.0-py3-none-any.bin.provenance\""),
        "{json}"
    );
    assert_eq!(serde_json::from_str::<UiFile>(&json).unwrap(), file);
}

#[test]
fn test_ui_file_omits_absent_provenance_from_the_wire() {
    let file = UiFile {
        filename: "pkg-1.0-py3-none-any.bin".to_owned(),
        release: None,
        url: "/alpha/files/aa/pkg-1.0-py3-none-any.bin".to_owned(),
        sha256: "aa".to_owned(),
        size: None,
        upload_time: None,
        yanked: false,
        yanked_reason: None,
        has_metadata: false,
        upstream: None,
        provenance: None,
        provenance_detail: None,
        source: UiArtifactSource::Hosted,
        availability: UiByteAvailability::Unavailable,
        browsable: false,
    };
    let json = serde_json::to_string(&file).unwrap();
    assert!(!json.contains("provenance"), "{json}");
    assert_eq!(serde_json::from_str::<UiFile>(&json).unwrap().provenance, None);
}

#[test]
fn test_ui_file_defaults_an_omitted_release_to_unassociated() {
    let file: UiFile = serde_json::from_value(serde_json::json!({
        "filename": "notes.txt",
        "url": "/files/notes.txt",
        "sha256": "aa",
        "size": null,
        "upload_time": null,
        "yanked": false,
        "yanked_reason": null,
        "has_metadata": false,
        "browsable": false,
        "source": "proxy",
        "availability": "remote_only",
    }))
    .unwrap();

    assert_eq!(file.release, None);
}
