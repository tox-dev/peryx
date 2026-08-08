use super::{
    CatalogGeneration, MetaStore, ProjectCachePurgeCounts, abort_catalog_generation, begin_catalog_generation,
    catalog_generation_prefix, catalog_state, freshness_key, list_catalog_projects, project_key,
    publish_catalog_generation, put_catalog_projects, recover_catalog_generations, refresh_catalog_generation,
};
use crate::store::PypiStore as _;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn generation(generation: u64, etag: Option<&str>, last_modified: Option<&str>) -> CatalogGeneration {
    CatalogGeneration {
        generation,
        source: "pypi".to_owned(),
        url: "https://pypi.org/simple/".to_owned(),
        format: "json".to_owned(),
        etag: etag.map(str::to_owned),
        last_modified: last_modified.map(str::to_owned),
        last_serial: Some(7),
        fetched_at_unix: 1,
        bytes: 100,
        projects: 2,
    }
}

#[test]
fn test_list_catalog_projects_is_bounded_and_canonical() {
    let (_dir, meta) = store();
    let (id, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
    put_catalog_projects(
        &meta,
        "pypi",
        id,
        &[
            ("zulu".to_owned(), "Zulu".to_owned()),
            ("alpha".to_owned(), "Alpha".to_owned()),
        ],
    )
    .unwrap();
    publish_catalog_generation(&meta, "pypi", expected, generation(id, None, None)).unwrap();

    assert_eq!(list_catalog_projects(&meta, "pypi", 1).unwrap(), vec!["alpha"]);
    assert!(list_catalog_projects(&meta, "missing", 1).unwrap().is_empty());
}

#[test]
fn test_put_and_list_projects_are_sorted_and_deduplicated() {
    let (_dir, meta) = store();
    assert!(meta.list_projects("root/pypi").unwrap().is_empty());
    meta.put_project("root/pypi", "flask", "Flask").unwrap();
    meta.put_project("root/pypi", "django", "Django").unwrap();
    meta.put_project("other/index", "x", "X").unwrap();
    meta.put_project("root/pypi", "flask", "Flask").unwrap();
    assert_eq!(meta.list_projects("root/pypi").unwrap(), vec!["Django", "Flask"]);
    assert_eq!(
        meta.get_project("root/pypi", "flask").unwrap().as_deref(),
        Some("Flask")
    );
}

#[test]
fn test_catalog_duplicates_are_order_independent_and_local_display_wins() {
    for projects in [
        [
            ("flask".to_owned(), "flask".to_owned()),
            ("flask".to_owned(), "Flask".to_owned()),
        ],
        [
            ("flask".to_owned(), "Flask".to_owned()),
            ("flask".to_owned(), "flask".to_owned()),
        ],
    ] {
        let (_dir, meta) = store();
        let (id, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
        assert_eq!(put_catalog_projects(&meta, "pypi", id, &projects).unwrap(), 1);
        publish_catalog_generation(&meta, "pypi", expected, generation(id, None, None)).unwrap();
        assert_eq!(meta.list_projects("pypi").unwrap(), vec!["Flask"]);
        meta.put_project("pypi", "flask", "Local-Flask").unwrap();
        assert_eq!(meta.list_projects("pypi").unwrap(), vec!["Local-Flask"]);
    }
}

#[test]
fn test_catalog_abort_and_stale_publish_only_touch_their_generation() {
    let (_dir, meta) = store();
    let (first, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
    put_catalog_projects(&meta, "pypi", first, &[("first".to_owned(), "first".to_owned())]).unwrap();
    let (second, _) = begin_catalog_generation(&meta, "pypi").unwrap();
    put_catalog_projects(&meta, "pypi", second, &[("second".to_owned(), "second".to_owned())]).unwrap();

    abort_catalog_generation(&meta, "pypi", first).unwrap();

    assert_eq!(catalog_state(&meta, "pypi").unwrap().staging, Some(second));
    assert!(
        meta.driver_prefix_keys(&catalog_generation_prefix("pypi", first))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        meta.driver_prefix_keys(&catalog_generation_prefix("pypi", second))
            .unwrap()
            .len(),
        1
    );
    assert!(publish_catalog_generation(&meta, "pypi", expected, generation(first, None, None)).is_err());
}

#[test]
fn test_catalog_batch_requires_its_staging_generation() {
    let (_dir, meta) = store();

    let error = put_catalog_projects(&meta, "pypi", 1, &[("flask".to_owned(), "Flask".to_owned())]).unwrap_err();

    assert!(matches!(error, peryx_storage::meta::MetaError::DriverPrecondition(_)));
}

#[test]
fn test_catalog_refresh_merges_present_validators() {
    let (_dir, meta) = store();
    let (id, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
    publish_catalog_generation(
        &meta,
        "pypi",
        expected,
        generation(id, Some("old-etag"), Some("old-date")),
    )
    .unwrap();

    refresh_catalog_generation(&meta, "pypi", id, None, Some("new-date".to_owned()), 9).unwrap();
    assert!(refresh_catalog_generation(&meta, "pypi", id + 1, None, None, 10).is_err());

    let active = catalog_state(&meta, "pypi").unwrap().active.unwrap();
    assert_eq!(active.etag.as_deref(), Some("old-etag"));
    assert_eq!(active.last_modified.as_deref(), Some("new-date"));
    assert_eq!(active.fetched_at_unix, 9);
}

#[test]
fn test_catalog_recovery_preserves_active_and_removes_pending_generations() {
    let (_dir, meta) = store();
    let (first, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
    put_catalog_projects(&meta, "pypi", first, &[("first".to_owned(), "first".to_owned())]).unwrap();
    publish_catalog_generation(&meta, "pypi", expected, generation(first, None, None)).unwrap();
    let (second, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
    put_catalog_projects(&meta, "pypi", second, &[("second".to_owned(), "second".to_owned())]).unwrap();
    publish_catalog_generation(&meta, "pypi", expected, generation(second, None, None)).unwrap();
    let (third, _) = begin_catalog_generation(&meta, "pypi").unwrap();
    put_catalog_projects(&meta, "pypi", third, &[("third".to_owned(), "third".to_owned())]).unwrap();

    recover_catalog_generations(&meta, "pypi").unwrap();

    let state = catalog_state(&meta, "pypi").unwrap();
    assert_eq!(state.active.unwrap().generation, second);
    assert_eq!(state.staging, None);
    assert_eq!(state.retired, None);
    assert!(
        meta.driver_prefix_keys(&catalog_generation_prefix("pypi", first))
            .unwrap()
            .is_empty()
    );
    assert!(
        meta.driver_prefix_keys(&catalog_generation_prefix("pypi", third))
            .unwrap()
            .is_empty()
    );
    assert_eq!(meta.list_projects("pypi").unwrap(), vec!["second"]);
}

#[test]
fn test_count_then_delete_project_cache_reports_and_removes_each_row() {
    let (_dir, meta) = store();
    let record = crate::store::CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 1,
        content_type: None,
        fresh_secs: None,
        body: Vec::new(),
    };
    let file_digests = vec!["a".repeat(64)];
    let metadata_digests = vec!["b".repeat(64)];
    meta.put_cached_page(
        "pypi/flask",
        &record,
        "pypi",
        "flask",
        "Flask",
        "pypi",
        None,
        Some("archived"),
        Some("read only"),
        &[(file_digests[0].clone(), "https://files/flask.whl".to_owned(), Some(123))],
        &[(
            metadata_digests[0].clone(),
            "https://files/flask.whl.metadata".to_owned(),
            "c".repeat(64),
        )],
        &[],
    )
    .unwrap();

    let expected = ProjectCachePurgeCounts {
        index_pages: 1,
        project_records: 1,
        project_status_records: 1,
        file_url_records: 1,
        metadata_records: 1,
        provenance_records: 0,
    };
    assert_eq!(
        meta.count_project_cache_purge("pypi", "flask", &file_digests, &metadata_digests)
            .unwrap(),
        expected
    );
    assert_eq!(
        meta.delete_project_cache("pypi", "flask", &file_digests, &metadata_digests)
            .unwrap(),
        expected
    );
    assert!(meta.get_index("pypi/flask").unwrap().is_none());
    assert!(meta.get_file_url(&file_digests[0]).unwrap().is_none());
    assert!(meta.get_metadata(&metadata_digests[0]).unwrap().is_none());
    assert!(meta.get_project_status("pypi", "flask").unwrap().is_none());
    assert!(meta.list_projects("pypi").unwrap().is_empty());
}

#[test]
fn test_delete_project_cache_counts_and_removes_the_provenance_row() {
    let (_dir, meta) = store();
    let file_digest = "a".repeat(64);
    meta.put_provenance(&file_digest, &"b".repeat(64), 16).unwrap();

    let counts = meta
        .delete_project_cache("pypi", "flask", std::slice::from_ref(&file_digest), &[])
        .unwrap();

    assert_eq!(counts.provenance_records, 1);
    assert!(meta.get_provenance(&file_digest).unwrap().is_none());
}

#[test]
fn test_delete_project_cache_removes_the_freshness_overlay() {
    let (_dir, meta) = store();
    let record = crate::store::CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 1,
        content_type: None,
        fresh_secs: None,
        body: Vec::new(),
    };
    meta.put_index("pypi/flask", &record).unwrap();
    meta.touch_index_freshness("pypi/flask", 42, Some(9)).unwrap();
    assert!(meta.get_driver_value(&freshness_key("pypi/flask")).unwrap().is_some());

    meta.delete_project_cache("pypi", "flask", &[], &[]).unwrap();

    assert!(meta.get_driver_value(&freshness_key("pypi/flask")).unwrap().is_none());
}

#[test]
fn test_scan_project_records_visits_valid_and_skips_non_utf8() {
    let (_dir, meta) = store();
    meta.put_project("pypi", "flask", "Flask").unwrap();
    meta.put_driver_value(&project_key("pypi", "bad"), &[0xff, 0xfe])
        .unwrap();
    let mut seen = Vec::new();
    meta.scan_project_records(|key, value| {
        seen.push((key.to_owned(), value.to_owned()));
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(seen, vec![("pypi/flask".to_owned(), "Flask".to_owned())]);
}
