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
}
