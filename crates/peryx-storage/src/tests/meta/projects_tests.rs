use super::{record, store};

#[test]
fn test_put_and_list_projects() {
    let (_dir, store) = store();
    assert!(store.list_projects("root/pypi").unwrap().is_empty());
    store.put_project("root/pypi", "flask", "Flask").unwrap();
    store.put_project("root/pypi", "django", "Django").unwrap();
    store.put_project("other/index", "x", "X").unwrap();
    store.put_project("root/pypi", "flask", "Flask").unwrap(); // re-observe, no duplicate
    assert_eq!(store.list_projects("root/pypi").unwrap(), vec!["Django", "Flask"]);
}

#[test]
fn test_count_and_delete_project_cache_purge() {
    let (_dir, store) = store();
    let file_digests = vec!["a".repeat(64)];
    let metadata_digests = vec!["b".repeat(64)];
    store
        .put_cached_page(
            "pypi/flask",
            &record(),
            "pypi",
            "flask",
            "Flask",
            "pypi",
            Some("archived"),
            Some("read only"),
            &[(
                file_digests[0].clone(),
                "https://files.example/flask.whl".to_owned(),
                Some(123),
            )],
            &[(
                metadata_digests[0].clone(),
                "https://files.example/flask.whl.metadata".to_owned(),
                "c".repeat(64),
            )],
        )
        .unwrap();
    assert_eq!(
        store.get_project_status("pypi", "flask").unwrap().unwrap(),
        crate::meta::ProjectStatusRecord {
            status: Some("archived".to_owned()),
            reason: Some("read only".to_owned()),
        }
    );

    assert_eq!(
        store
            .count_project_cache_purge("pypi", "flask", &file_digests, &metadata_digests)
            .unwrap(),
        crate::meta::ProjectCachePurgeCounts {
            index_pages: 1,
            project_records: 1,
            project_status_records: 1,
            file_url_records: 1,
            metadata_records: 1,
        }
    );
    assert_eq!(
        store
            .delete_project_cache("pypi", "flask", &file_digests, &metadata_digests)
            .unwrap(),
        crate::meta::ProjectCachePurgeCounts {
            index_pages: 1,
            project_records: 1,
            project_status_records: 1,
            file_url_records: 1,
            metadata_records: 1,
        }
    );
    assert!(store.get_index("pypi/flask").unwrap().is_none());
    assert!(store.get_file_url("a".repeat(64).as_str()).unwrap().is_none());
    assert!(store.get_metadata("b".repeat(64).as_str()).unwrap().is_none());
    assert!(store.get_project_status("pypi", "flask").unwrap().is_none());
    assert!(store.list_projects("pypi").unwrap().is_empty());
}
