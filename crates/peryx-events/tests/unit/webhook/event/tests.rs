use super::*;

#[test]
fn test_event_names_roundtrip() {
    for (kind, name) in [
        (WebhookEventKind::Upload, "upload"),
        (WebhookEventKind::Yank, "yank"),
        (WebhookEventKind::Unyank, "unyank"),
        (WebhookEventKind::Delete, "delete"),
        (WebhookEventKind::Restore, "restore"),
        (WebhookEventKind::Promote, "promote"),
        (WebhookEventKind::ProjectStatus, "project-status"),
        (WebhookEventKind::Management, "management"),
    ] {
        assert_eq!(kind.as_str(), name);
        assert_eq!(WebhookEventKind::parse(name), Some(kind));
    }
    assert_eq!(WebhookEventKind::parse("unknown"), None);
}

#[test]
fn test_event_payload_serializes_file_and_context() {
    let event = WebhookEvent {
        kind: WebhookEventKind::Upload,
        created_at_unix: 10,
        index: "virtual".to_owned(),
        route: "route".to_owned(),
        hosted_index: "hosted".to_owned(),
        project: "demo".to_owned(),
        version: Some("1.0".to_owned()),
        filename: Some("demo.whl".to_owned()),
        digest: Some("sha256".to_owned()),
        count: 2,
        actor: Some("alice".to_owned()),
        request_id: Some("request-1".to_owned()),
    };

    assert_eq!(
        serde_json::to_value(event.payload()).unwrap(),
        serde_json::json!({
            "event": "upload",
            "created_at": 10,
            "index": "virtual",
            "route": "route",
            "hosted_index": "hosted",
            "project": "demo",
            "version": "1.0",
            "file": {"filename": "demo.whl", "sha256": "sha256"},
            "count": 2,
            "actor": "alice",
            "request_id": "request-1",
        })
    );
}

#[test]
fn test_event_payload_omits_absent_optional_context() {
    let payload = serde_json::to_value(
        WebhookEvent {
            kind: WebhookEventKind::Delete,
            created_at_unix: 10,
            index: "hosted".to_owned(),
            route: "hosted".to_owned(),
            hosted_index: "hosted".to_owned(),
            project: "demo".to_owned(),
            version: None,
            filename: None,
            digest: None,
            count: 1,
            actor: None,
            request_id: None,
        }
        .payload(),
    )
    .unwrap();

    assert_eq!(payload.get("version"), None);
    assert_eq!(payload.get("file"), None);
    assert_eq!(payload.get("actor"), None);
    assert_eq!(payload.get("request_id"), None);
}
