use super::store;

#[test]
fn test_summarize_indexes_counts_projects_and_recent_uploads() {
    let (_dir, store) = store();
    store.put_project("hosted", "flask", "Flask").unwrap();
    store.put_project("root/hosted", "django", "Django").unwrap();
    store
        .put_upload(
            "hosted",
            "flask",
            "flask-1.0.whl",
            br#"{"version":"1.0","file":{"filename":"flask-1.0.whl","upload-time":"2026-01-01T00:00:00Z","size":10}}"#,
        )
        .unwrap();
    store
        .put_upload(
            "root/hosted",
            "django",
            "django-4.0.whl",
            br#"{"version":"4.0","file":{"filename":"django-4.0.whl","upload-time":"2026-02-01T00:00:00Z","size":20}}"#,
        )
        .unwrap();
    store
        .put_upload(
            "root/hosted",
            "django",
            "django-4.1.whl",
            br#"{"version":"4.1","file":{"filename":"django-4.1.whl","upload-time":"2026-02-01T00:00:00Z","size":21}}"#,
        )
        .unwrap();
    store
        .put_upload(
            "root/hosted",
            "django",
            "django-3.2.whl",
            br#"{"version":"3.2","file":{"filename":"django-3.2.whl","upload-time":"2025-12-01T00:00:00Z","size":15}}"#,
        )
        .unwrap();
    store
        .put_upload("foreign", "flask", "ignored.whl", br#"{"version":"1.0"}"#)
        .unwrap();

    let indexes = vec!["hosted".to_owned(), "root/hosted".to_owned()];
    let summary = store.summarize_indexes(&indexes, 1).unwrap();

    assert_eq!(summary["hosted"].project_count, 1);
    assert_eq!(summary["hosted"].upload_count, 1);
    assert_eq!(summary["root/hosted"].project_count, 1);
    assert_eq!(summary["root/hosted"].upload_count, 3);
    assert_eq!(summary["root/hosted"].recent_uploads[0].filename, "django-4.0.whl");

    let summary = store.summarize_indexes(&indexes, 0).unwrap();
    assert_eq!(summary["root/hosted"].upload_count, 3);
    assert!(summary["root/hosted"].recent_uploads.is_empty());
}
