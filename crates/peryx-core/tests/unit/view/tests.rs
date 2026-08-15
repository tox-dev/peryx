use super::{
    BrowseBadge, BrowseCell, BrowseLink, BrowsePage, BrowseProperty, BrowseRow, BrowseSection, UiAction,
    UiActionMethod, UiArtifactSource, UiByteAvailability, UiOperationStatus,
};
use rstest::rstest;

#[rstest]
#[case::put(UiActionMethod::Put, "\"put\"", "PUT")]
#[case::post(UiActionMethod::Post, "\"post\"", "POST")]
#[case::delete(UiActionMethod::Delete, "\"delete\"", "DELETE")]
fn action_methods_use_http_spelling_and_round_trip(
    #[case] method: UiActionMethod,
    #[case] json: &str,
    #[case] http: &str,
) {
    assert_eq!(
        (
            serde_json::to_string(&method).unwrap(),
            serde_json::from_str::<UiActionMethod>(json).unwrap(),
            method.as_str(),
        ),
        (json.to_owned(), method, http)
    );
}

#[test]
fn browse_page_round_trips_all_section_shapes() {
    let page = BrowsePage {
        breadcrumbs: vec![BrowseLink {
            label: "Index".to_owned(),
            href: "/+ui/".to_owned(),
        }],
        title: "Artifact".to_owned(),
        subtitle: Some("Build output".to_owned()),
        summary: Some("Signed and replicated".to_owned()),
        command: Some("client fetch artifact".to_owned()),
        badges: vec![BrowseBadge {
            label: "local".to_owned(),
            class: "available".to_owned(),
            hint: Some("Bytes are stored on this node".to_owned()),
        }],
        sections: vec![
            BrowseSection::Markup {
                heading: "Description".to_owned(),
                html: "<p>Artifact</p>".to_owned(),
                notice: Some("Sanitized".to_owned()),
            },
            BrowseSection::Properties {
                heading: "Properties".to_owned(),
                entries: vec![BrowseProperty {
                    label: "Digest".to_owned(),
                    value: "abc123".to_owned(),
                    href: Some("/+ui/artifact?digest=abc123".to_owned()),
                }],
            },
            BrowseSection::Links {
                heading: "Related".to_owned(),
                entries: vec![BrowseLink {
                    label: "Metadata".to_owned(),
                    href: "/+ui/metadata".to_owned(),
                }],
                empty: "No related records".to_owned(),
            },
            BrowseSection::Table {
                heading: "Files".to_owned(),
                columns: vec!["Name".to_owned()],
                rows: vec![BrowseRow {
                    cells: vec![BrowseCell {
                        text: "artifact.bin".to_owned(),
                        href: Some("/files/artifact.bin".to_owned()),
                        code: true,
                    }],
                    badges: vec![BrowseBadge {
                        label: "ready".to_owned(),
                        class: "success".to_owned(),
                        hint: None,
                    }],
                    actions: vec![UiAction {
                        label: "Remove".to_owned(),
                        method: UiActionMethod::Delete,
                        endpoint: "/files/artifact.bin".to_owned(),
                        destructive: true,
                    }],
                }],
                empty: "No files".to_owned(),
            },
            BrowseSection::Content {
                heading: "Preview".to_owned(),
                text: "content".to_owned(),
                size: Some(7),
                offset: 0,
                next: Some(BrowseLink {
                    label: "Next".to_owned(),
                    href: "/+ui/preview?offset=7".to_owned(),
                }),
            },
        ],
        actions: vec![UiAction {
            label: "Refresh".to_owned(),
            method: UiActionMethod::Post,
            endpoint: "/+ui/refresh".to_owned(),
            destructive: false,
        }],
    };

    assert_eq!(
        serde_json::from_value::<BrowsePage>(serde_json::to_value(&page).unwrap()).unwrap(),
        page
    );
}

#[test]
fn browse_defaults_have_compact_wire_format() {
    assert_eq!(
        serde_json::to_value(BrowsePage::default()).unwrap(),
        serde_json::json!({"title": ""})
    );
    assert_eq!(
        serde_json::to_value(BrowseRow::default()).unwrap(),
        serde_json::json!({})
    );
    assert_eq!(
        serde_json::to_value(BrowseCell::default()).unwrap(),
        serde_json::json!({"text": "", "code": false})
    );
}

#[rstest]
#[case::published(true, false, Some(10), 5, UiOperationStatus::Published)]
#[case::failed(false, true, None, 5, UiOperationStatus::Failed)]
#[case::expired(false, false, Some(10), 10, UiOperationStatus::Expired)]
#[case::before_expiry(false, false, Some(10), 9, UiOperationStatus::Pending)]
#[case::no_expiry(false, false, None, 9, UiOperationStatus::Pending)]
fn operation_status_uses_durable_terminal_state_before_expiry(
    #[case] published: bool,
    #[case] failed: bool,
    #[case] expiry: Option<i64>,
    #[case] now: i64,
    #[case] expected: UiOperationStatus,
) {
    assert_eq!(
        (
            UiOperationStatus::derive(published, failed, expiry, now),
            serde_json::from_str::<UiOperationStatus>(&serde_json::to_string(&expected).unwrap()).unwrap(),
            expected.as_str(),
        ),
        (
            expected,
            expected,
            serde_json::to_value(expected).unwrap().as_str().unwrap(),
        )
    );
}

#[test]
fn artifact_sources_use_stable_wire_names() {
    for (source, expected) in [
        (UiArtifactSource::Hosted, "hosted"),
        (UiArtifactSource::Proxy, "proxy"),
        (UiArtifactSource::Generated, "generated"),
    ] {
        assert_eq!(source.as_str(), expected);
        assert_eq!(serde_json::to_value(source).unwrap(), expected);
    }
}

#[test]
fn byte_availability_uses_stable_wire_names() {
    for (availability, expected) in [
        (UiByteAvailability::Local, "local"),
        (UiByteAvailability::RemoteOnly, "remote_only"),
        (UiByteAvailability::Unavailable, "unavailable"),
    ] {
        assert_eq!(availability.as_str(), expected);
        assert_eq!(serde_json::to_value(availability).unwrap(), expected);
    }
}
