use crate::view::{
    AttestationView, FileView, LifecycleView, ProjectStatusView, ProjectView, ProvenanceSource, ProvenanceView,
    ReleaseView, SubjectMatch,
};
use peryx_core::{UiAction, UiActionMethod, UiArtifactSource, UiByteAvailability};

#[test]
fn provenance_enums_use_snake_case() {
    for (value, wire) in [
        (ProvenanceSource::Hosted, "\"hosted\""),
        (ProvenanceSource::Mirrored, "\"mirrored\""),
    ] {
        assert_eq!(serde_json::to_string(&value).unwrap(), wire);
        assert_eq!(serde_json::from_str::<ProvenanceSource>(wire).unwrap(), value);
    }
    for (value, wire) in [
        (SubjectMatch::Matched, "\"matched\""),
        (SubjectMatch::Mismatched, "\"mismatched\""),
        (SubjectMatch::Unknown, "\"unknown\""),
    ] {
        assert_eq!(serde_json::to_string(&value).unwrap(), wire);
        assert_eq!(serde_json::from_str::<SubjectMatch>(wire).unwrap(), value);
    }
}

#[test]
fn provenance_round_trips_attestation_details() {
    let provenance = ProvenanceView {
        source: ProvenanceSource::Hosted,
        attestations: vec![AttestationView {
            predicate_type: Some("https://docs.example/attestations/publish/v1".to_owned()),
            subject: SubjectMatch::Matched,
        }],
        malformed: true,
    };

    assert_eq!(
        serde_json::from_value::<ProvenanceView>(serde_json::to_value(&provenance).unwrap()).unwrap(),
        provenance
    );
}

#[test]
fn project_round_trips_release_lifecycle_and_actions() {
    let project = ProjectView {
        name: "sample".to_owned(),
        status: Some(Box::new(ProjectStatusView {
            marker: "quarantined".to_owned(),
            reason: Some("policy".to_owned()),
        })),
        versions: vec![ReleaseView {
            version: "1.0".to_owned(),
            lifecycle: Some(LifecycleView {
                label: "withdrawn".to_owned(),
                reasons: vec!["superseded".to_owned()],
            }),
            actions: vec![UiAction {
                label: "Restore".to_owned(),
                method: UiActionMethod::Put,
                endpoint: "/alpha/sample/1.0/restore".to_owned(),
                destructive: false,
            }],
        }],
        files: Vec::new(),
        actions: Vec::new(),
        client_command: Some("pip install sample".to_owned()),
    };

    assert_eq!(
        serde_json::from_value::<ProjectView>(serde_json::to_value(&project).unwrap()).unwrap(),
        project
    );
}

#[test]
fn file_round_trips_distribution_state() {
    let file = FileView {
        filename: "sample-1.0-py3-none-any.whl".to_owned(),
        release: Some("1.0".to_owned()),
        url: "/alpha/files/aa/sample-1.0-py3-none-any.whl".to_owned(),
        sha256: "aa".to_owned(),
        size: Some(10),
        upload_time: None,
        lifecycle: None,
        has_metadata: false,
        browsable: true,
        provenance: Some("/alpha/files/aa/sample-1.0-py3-none-any.whl.provenance".to_owned()),
        provenance_detail: Some(ProvenanceView {
            source: ProvenanceSource::Mirrored,
            attestations: Vec::new(),
            malformed: false,
        }),
        upstream: Some("mirror".to_owned()),
        source: UiArtifactSource::Proxy,
        availability: UiByteAvailability::RemoteOnly,
    };

    assert_eq!(
        serde_json::from_value::<FileView>(serde_json::to_value(&file).unwrap()).unwrap(),
        file
    );
}
