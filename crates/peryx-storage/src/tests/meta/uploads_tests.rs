use super::store;

#[test]
fn test_put_and_list_upload_entries() {
    let (_dir, store) = store();
    assert!(store.list_upload_entries("root/hosted", "flask").unwrap().is_empty());
    store
        .put_upload("root/hosted", "flask", "flask-2.0.whl", b"two")
        .unwrap();
    store
        .put_upload("root/hosted", "flask", "flask-1.0.whl", b"one")
        .unwrap();
    store
        .put_upload("root/hosted", "django", "django-4.0.whl", b"other")
        .unwrap();
    // Sorted by filename, scoped to the one project, filename returned alongside the record.
    assert_eq!(
        store.list_upload_entries("root/hosted", "flask").unwrap(),
        vec![
            ("flask-1.0.whl".to_owned(), b"one".to_vec()),
            ("flask-2.0.whl".to_owned(), b"two".to_vec())
        ]
    );
    assert_eq!(
        store.list_upload_entries("root/hosted", "django").unwrap(),
        vec![("django-4.0.whl".to_owned(), b"other".to_vec())]
    );
}

#[test]
fn test_put_uploads_stores_records_and_project_display() {
    let (_dir, store) = store();
    let records = vec![
        ("flask-1.0.whl".to_owned(), b"one".to_vec()),
        ("flask-1.0.tar.gz".to_owned(), b"sdist".to_vec()),
    ];

    store.put_uploads("prod", "flask", "Flask", &records).unwrap();

    assert_eq!(store.get_project("prod", "flask").unwrap().as_deref(), Some("Flask"));
    assert_eq!(
        store.list_upload_entries("prod", "flask").unwrap(),
        vec![
            ("flask-1.0.tar.gz".to_owned(), b"sdist".to_vec()),
            ("flask-1.0.whl".to_owned(), b"one".to_vec())
        ]
    );
}

#[test]
fn test_put_upload_overwrites_same_filename() {
    let (_dir, store) = store();
    store
        .put_upload("root/hosted", "flask", "flask-1.0.whl", b"first")
        .unwrap();
    store
        .put_upload("root/hosted", "flask", "flask-1.0.whl", b"second")
        .unwrap();
    assert_eq!(
        store.list_upload_entries("root/hosted", "flask").unwrap(),
        vec![("flask-1.0.whl".to_owned(), b"second".to_vec())]
    );
}

#[test]
fn test_get_upload_fetches_one_entry() {
    let (_dir, store) = store();
    store
        .put_upload("root/hosted", "flask", "flask-1.0.whl", b"one")
        .unwrap();
    assert_eq!(
        store
            .get_upload("root/hosted", "flask", "flask-1.0.whl")
            .unwrap()
            .as_deref(),
        Some(b"one".as_slice())
    );
    assert!(
        store
            .get_upload("root/hosted", "flask", "missing.whl")
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_delete_upload() {
    let (_dir, store) = store();
    store
        .put_upload("root/hosted", "flask", "flask-1.0.whl", b"one")
        .unwrap();
    assert!(store.delete_upload("root/hosted", "flask", "flask-1.0.whl").unwrap());
    assert!(!store.delete_upload("root/hosted", "flask", "flask-1.0.whl").unwrap());
    assert!(store.list_upload_entries("root/hosted", "flask").unwrap().is_empty());
}

#[test]
fn test_put_list_and_delete_overrides() {
    let (_dir, store) = store();
    assert!(store.list_overrides("hosted", "flask").unwrap().is_empty());
    store
        .put_override("hosted", "flask", "flask-1.0.whl", "yanked")
        .unwrap();
    store
        .put_override("hosted", "flask", "flask-2.0.whl", "hidden")
        .unwrap();
    store.put_override("hosted", "other", "x.whl", "hidden").unwrap();
    assert_eq!(
        store.list_overrides("hosted", "flask").unwrap(),
        vec![
            ("flask-1.0.whl".to_owned(), "yanked".to_owned()),
            ("flask-2.0.whl".to_owned(), "hidden".to_owned())
        ]
    );
    assert!(store.delete_override("hosted", "flask", "flask-1.0.whl").unwrap());
    assert!(!store.delete_override("hosted", "flask", "flask-1.0.whl").unwrap());
}

#[test]
fn test_put_override_replaces_kind() {
    let (_dir, store) = store();
    store
        .put_override("hosted", "flask", "flask-1.0.whl", "yanked")
        .unwrap();
    store
        .put_override("hosted", "flask", "flask-1.0.whl", "hidden")
        .unwrap();
    assert_eq!(
        store.list_overrides("hosted", "flask").unwrap(),
        vec![("flask-1.0.whl".to_owned(), "hidden".to_owned())]
    );
}

#[test]
fn test_scan_upload_and_override_records_visit_rows() {
    let (_dir, store) = store();
    store.put_upload("hosted", "flask", "flask-1.0.whl", b"upload").unwrap();
    store
        .put_override("hosted", "flask", "flask-1.0.whl", "hidden")
        .unwrap();

    let mut uploads = Vec::new();
    store
        .scan_upload_records(|key, value| {
            uploads.push((key.to_owned(), value.to_vec()));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
    assert_eq!(
        uploads,
        vec![("hosted/flask/flask-1.0.whl".to_owned(), b"upload".to_vec())]
    );

    let mut overrides = Vec::new();
    store
        .scan_override_records(|key, value| {
            overrides.push((key.to_owned(), value.to_owned()));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
    assert_eq!(
        overrides,
        vec![("hosted/flask/flask-1.0.whl".to_owned(), "hidden".to_owned())]
    );
}
