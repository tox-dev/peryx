use super::{UiHosted, UiSnapshot, UiSummaryStatus, UiUpstream};

#[test]
fn status_snapshot_reads_complete_index_state() {
    let snapshot = UiSnapshot::from_status(&serde_json::json!({
        "version": "1", "serial": 2, "requests": 3,
        "by_ecosystem": [], "metric_families": [],
        "indexes": [{
            "name": "hosted", "route": "root/hosted", "ecosystem": "example", "endpoint": "/root/hosted/",
            "kind": "hosted", "layers": ["cache", 7], "uploads": true,
            "upload": null, "upload_to": "hosted",
            "upstream": {"url": "https://example.invalid/", "auth": {"kind": "basic", "redacted": "u:***"}, "status": "configured"},
            "hosted": {"volatile": true, "upload_token": {"configured": true, "redacted": "***"}},
            "summary": {"status": "available"},
            "resource_count": 4, "write_count": 5,
            "recent_writes": [{"resource": "artifact", "artifact": "artifact.bin", "version": "1", "written_at": "now", "size": 6}]
        }]
    }));
    assert_eq!(
        snapshot.indexes[0].upstream,
        Some(UiUpstream {
            url: "https://example.invalid/".to_owned(),
            auth_kind: "basic".to_owned(),
            auth_redacted: Some("u:***".to_owned()),
            status: "configured".to_owned(),
        })
    );
    assert_eq!(
        snapshot.indexes[0].hosted,
        Some(UiHosted {
            volatile: true,
            token_configured: true,
            token_redacted: Some("***".to_owned()),
        })
    );
    assert_eq!(snapshot.indexes[0].layers, ["cache"]);
    assert_eq!(snapshot.indexes[0].summary_status, UiSummaryStatus::Available);
    assert_eq!(snapshot.indexes[0].summary_error_class, None);
    assert_eq!(snapshot.indexes[0].recent_writes[0].size, Some(6));
}

#[test]
fn status_snapshot_defaults_absent_and_malformed_fields() {
    let snapshot = UiSnapshot::from_status(&serde_json::json!({
        "indexes": [
            {"upstream": 7, "hosted": false},
            {"summary": {"status": "unavailable", "error_class": "storage"}}
        ]
    }));
    assert_eq!(snapshot.version, "");
    assert_eq!(snapshot.indexes.len(), 2);
    assert_eq!(snapshot.indexes[0].upstream, None);
    assert_eq!(snapshot.indexes[0].hosted, None);
    assert_eq!(snapshot.indexes[0].summary_status, UiSummaryStatus::Unsupported);
    assert_eq!(snapshot.indexes[1].summary_status, UiSummaryStatus::Unavailable);
    assert_eq!(snapshot.indexes[1].summary_error_class.as_deref(), Some("storage"));
}
