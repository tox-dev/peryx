use std::collections::BTreeMap;

use rstest::rstest;

use super::StatusDocument;
use crate::data::{LoaderEndpoint, LoaderError};
use crate::model::{
    UiEcosystemSummary, UiHosted, UiIndex, UiMetricFamily, UiRecentWrite, UiSnapshot, UiSummaryStatus, UiUpstream,
};

fn hosted_index(summary: &serde_json::Value) -> serde_json::Value {
    let mut index = serde_json::json!({
        "name": "hosted",
        "route": "root/hosted",
        "ecosystem": "pypi",
        "endpoint": "/root/hosted/simple/",
        "kind": "hosted",
        "layers": [],
        "uploads": true,
        "upload_to": null,
        "upstream": null,
        "hosted": {"volatile": true, "upload_token": {"configured": true, "redacted": "tok***"}},
    });
    index
        .as_object_mut()
        .unwrap()
        .extend(summary.as_object().unwrap().clone());
    index
}

fn indexes_of(index: &serde_json::Value) -> Result<Vec<UiIndex>, LoaderError> {
    let document: StatusDocument = serde_json::from_value(serde_json::json!({
        "version": "1.2.3",
        "indexes": [index],
    }))
    .unwrap();
    UiSnapshot::try_from(document).map(|snapshot| snapshot.indexes)
}

fn hosted_ui_index(summary_status: UiSummaryStatus, summary_error_class: Option<&str>) -> UiIndex {
    UiIndex {
        name: "hosted".to_owned(),
        route: "root/hosted".to_owned(),
        ecosystem: "pypi".to_owned(),
        endpoint: "/root/hosted/simple/".to_owned(),
        kind: "hosted".to_owned(),
        layers: Vec::new(),
        uploads: true,
        upload_to: None,
        upstream: None,
        hosted: Some(UiHosted {
            volatile: true,
            token_configured: true,
            token_redacted: Some("tok***".to_owned()),
        }),
        summary_status,
        summary_error_class: summary_error_class.map(str::to_owned),
        resource_count: 0,
        write_count: 0,
        recent_writes: Vec::new(),
    }
}

#[test]
fn status_document_converts_every_snapshot_field() {
    let document: StatusDocument = serde_json::from_value(serde_json::json!({
        "version": "1.2.3",
        "serial": 42,
        "requests": 1234,
        "by_ecosystem": [{
            "ecosystem": "pypi",
            "pages": 1,
            "reads": 2,
            "bytes": 3,
            "rejected": 4,
            "writes": 5,
            "families": {"metadata": 6},
        }],
        "metric_families": [{
            "ecosystem": "pypi",
            "key": "metadata",
            "label": "Metadata",
            "roles": ["cached", "hosted"],
        }],
        "indexes": [],
    }))
    .unwrap();

    assert_eq!(
        UiSnapshot::try_from(document),
        Ok(UiSnapshot {
            version: "1.2.3".to_owned(),
            serial: Some(42),
            requests: 1234,
            ecosystems: vec![UiEcosystemSummary {
                ecosystem: "pypi".to_owned(),
                pages: 1,
                reads: 2,
                bytes: 3,
                rejected: 4,
                writes: 5,
                families: BTreeMap::from([("metadata".to_owned(), 6)]),
            }],
            families: vec![UiMetricFamily {
                ecosystem: "pypi".to_owned(),
                key: "metadata".to_owned(),
                label: "Metadata".to_owned(),
                roles: vec!["cached".to_owned(), "hosted".to_owned()],
            }],
            indexes: Vec::new(),
        })
    );
}

#[test]
fn status_index_with_available_summary_converts_every_field() {
    let index: serde_json::Value = serde_json::json!({
        "name": "all",
        "route": "root/all",
        "ecosystem": "pypi",
        "endpoint": "/root/all/simple/",
        "kind": "virtual",
        "layers": ["hosted", "cache"],
        "uploads": true,
        "upload_to": "hosted",
        "upstream": {
            "url": "https://pypi.org/simple/",
            "auth": {"kind": "basic", "redacted": "user:***"},
            "status": "healthy",
        },
        "hosted": {"volatile": false, "upload_token": {"configured": true, "redacted": "tok***"}},
        "summary": {"status": "available"},
        "resource_count": 7,
        "write_count": 9,
        "recent_writes": [
            {
                "resource": "demo",
                "artifact": "demo-1.0.whl",
                "group": "1.0",
                "written_at": "2026-01-02T03:04:05Z",
                "size": 2048,
            },
            {"resource": "bare", "artifact": "bare-2.0.whl", "group": "2.0", "written_at": null, "size": null},
        ],
    });

    assert_eq!(
        indexes_of(&index),
        Ok(vec![UiIndex {
            name: "all".to_owned(),
            route: "root/all".to_owned(),
            ecosystem: "pypi".to_owned(),
            endpoint: "/root/all/simple/".to_owned(),
            kind: "virtual".to_owned(),
            layers: vec!["hosted".to_owned(), "cache".to_owned()],
            uploads: true,
            upload_to: Some("hosted".to_owned()),
            upstream: Some(UiUpstream {
                url: "https://pypi.org/simple/".to_owned(),
                auth_kind: "basic".to_owned(),
                auth_redacted: Some("user:***".to_owned()),
                status: "healthy".to_owned(),
            }),
            hosted: Some(UiHosted {
                volatile: false,
                token_configured: true,
                token_redacted: Some("tok***".to_owned()),
            }),
            summary_status: UiSummaryStatus::Available,
            summary_error_class: None,
            resource_count: 7,
            write_count: 9,
            recent_writes: vec![
                UiRecentWrite {
                    resource: "demo".to_owned(),
                    artifact: "demo-1.0.whl".to_owned(),
                    group: "1.0".to_owned(),
                    written_at: Some("2026-01-02T03:04:05Z".to_owned()),
                    size: Some(2048),
                },
                UiRecentWrite {
                    resource: "bare".to_owned(),
                    artifact: "bare-2.0.whl".to_owned(),
                    group: "2.0".to_owned(),
                    written_at: None,
                    size: None,
                },
            ],
        }])
    );
}

#[test]
fn status_document_without_optional_fields_uses_empty_defaults() {
    let document: StatusDocument = serde_json::from_str(r#"{"version": "1.2.3", "indexes": []}"#).unwrap();

    assert_eq!(
        UiSnapshot::try_from(document),
        Ok(UiSnapshot {
            version: "1.2.3".to_owned(),
            ..UiSnapshot::default()
        })
    );
}

#[rstest]
#[case::unavailable(
    serde_json::json!({"summary": {"status": "unavailable", "error_class": "storage"}}),
    UiSummaryStatus::Unavailable,
    Some("storage")
)]
#[case::unsupported(serde_json::json!({"summary": {"status": "unsupported"}}), UiSummaryStatus::Unsupported, None)]
#[case::absent(serde_json::json!({}), UiSummaryStatus::Unsupported, None)]
fn status_index_without_available_summary_reports_no_counts(
    #[case] summary: serde_json::Value,
    #[case] status: UiSummaryStatus,
    #[case] error_class: Option<&str>,
) {
    assert_eq!(
        indexes_of(&hosted_index(&summary)),
        Ok(vec![hosted_ui_index(status, error_class)])
    );
}

#[test]
fn status_index_ignores_counts_beside_an_unavailable_summary() {
    let summary: serde_json::Value = serde_json::json!({
        "summary": {"status": "unavailable", "error_class": "storage"},
        "resource_count": 7,
        "write_count": 9,
        "recent_writes": [{"resource": "demo", "artifact": "a", "group": "1", "written_at": null, "size": null}],
    });

    assert_eq!(
        indexes_of(&hosted_index(&summary)),
        Ok(vec![hosted_ui_index(UiSummaryStatus::Unavailable, Some("storage"))])
    );
}

#[rstest]
#[case::resource_count(serde_json::json!({"write_count": 9, "recent_writes": []}))]
#[case::write_count(serde_json::json!({"resource_count": 7, "recent_writes": []}))]
#[case::recent_writes(serde_json::json!({"resource_count": 7, "write_count": 9}))]
fn status_index_with_available_summary_requires_each_count(#[case] mut counts: serde_json::Value) {
    counts["summary"] = serde_json::json!({"status": "available"});

    assert_eq!(
        indexes_of(&hosted_index(&counts)),
        Err(LoaderError::Invalid(LoaderEndpoint::Status))
    );
}
