use super::{
    CatalogGeneration, MetaStore, ProjectCachePurgeCounts, abort_catalog_generation, begin_catalog_generation,
    catalog_generation_prefix, catalog_projects_in_snapshot, catalog_state, freshness_key, list_catalog_projects,
    publish_catalog_generation, put_catalog_projects, recover_catalog_generations, refresh_catalog_generation,
};
use crate::store::PypiStore as _;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn uninitialized_store() -> (tempfile::TempDir, MetaStore) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    drop(redb::Database::create(&path).unwrap());
    (directory, MetaStore::open_existing(path).unwrap())
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
fn test_list_catalog_projects_keeps_one_generation_when_publication_reclaims_mid_read() {
    let (_dir, meta) = store();
    let (retired, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
    put_catalog_projects(
        &meta,
        "pypi",
        retired,
        &[
            ("alpha".to_owned(), "Alpha".to_owned()),
            ("bravo".to_owned(), "Bravo".to_owned()),
        ],
    )
    .unwrap();
    publish_catalog_generation(&meta, "pypi", expected, generation(retired, None, None)).unwrap();

    let during = meta
        .read_driver_txn(|txn| {
            let (replacement, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
            put_catalog_projects(
                &meta,
                "pypi",
                replacement,
                &[("charlie".to_owned(), "Charlie".to_owned())],
            )
            .unwrap();
            publish_catalog_generation(&meta, "pypi", expected, generation(replacement, None, None)).unwrap();
            recover_catalog_generations(&meta, "pypi").unwrap();
            catalog_projects_in_snapshot(txn, "pypi", usize::MAX)
        })
        .unwrap();

    assert_eq!(during, vec!["alpha", "bravo"]);
    assert_eq!(
        list_catalog_projects(&meta, "pypi", usize::MAX).unwrap(),
        vec!["charlie"]
    );
}

#[test]
fn test_put_and_list_projects_are_sorted_and_deduplicated() {
    let (_dir, meta) = store();
    assert!(meta.list_projects("root-pypi").unwrap().is_empty());
    meta.put_project("root-pypi", "flask", "Flask").unwrap();
    meta.put_project("root-pypi", "django", "Django").unwrap();
    meta.put_project("other", "x", "X").unwrap();
    meta.put_project("root-pypi", "flask", "Flask").unwrap();
    assert_eq!(meta.list_projects("root-pypi").unwrap(), vec!["Django", "Flask"]);
    assert_eq!(
        meta.get_project("root-pypi", "flask").unwrap().as_deref(),
        Some("Flask")
    );
}

#[test]
fn test_list_projects_reports_a_missing_driver_table() {
    let (_directory, meta) = uninitialized_store();

    assert!(meta.list_projects("pypi").is_err());
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
        source: None,
        last_modified: None,
        etag: None,
        last_serial: None,
        fetched_at_unix: 1,
        content_type: None,
        fresh_secs: None,
        body: Vec::new(),
    };
    let file_digests = vec!["a".repeat(64)];
    let metadata_digests = vec!["b".repeat(64)];
    meta.put_cached_page(crate::store::CachedPageWrite {
        key: "pypi/flask",
        record: &record,
        index: "pypi",
        normalized: "flask",
        display: "Flask",
        source: "pypi",
        upstream: None,
        project_status: Some("archived"),
        project_status_reason: Some("read only"),
        files: &[crate::store::PublishedFileWrite {
            sha256: file_digests[0].clone(),
            filename: "flask-1.0.whl".to_owned(),
            url: "https://files/flask.whl".to_owned(),
            size: Some(123),
            metadata: Some(("https://files/flask.whl.metadata".to_owned(), "c".repeat(64))),
        }],
        attestations: &[],
    })
    .unwrap();

    meta.put_metadata(&metadata_digests[0], &"c".repeat(64)).unwrap();

    let expected = ProjectCachePurgeCounts {
        index_pages: 1,
        project_records: 1,
        project_status_records: 1,
        file_url_records: 1,
        metadata_records: 1,
    };
    assert_eq!(
        meta.count_project_cache_purge("pypi", "flask", &metadata_digests)
            .unwrap(),
        expected
    );
    assert_eq!(
        meta.delete_project_cache("pypi", "flask", &metadata_digests).unwrap(),
        expected
    );
    assert!(meta.get_index("pypi/flask").unwrap().is_none());
    assert!(meta.get_file_url("pypi", "flask", &file_digests[0]).unwrap().is_none());
    assert!(meta.get_metadata_digest(&metadata_digests[0]).unwrap().is_none());
    assert_eq!(
        meta.get_file_publication("pypi", "flask", &file_digests[0], "flask-1.0.whl")
            .unwrap(),
        None,
        "the purge drops the project's publication rows"
    );
    assert!(meta.get_project_status("pypi", "flask").unwrap().is_none());
    assert!(meta.list_projects("pypi").unwrap().is_empty());
}

#[test]
fn test_delete_project_cache_leaves_a_hosted_publications_bundle_alone() {
    let (_dir, meta) = store();
    let file_digest = "a".repeat(64);
    let bundle = "b".repeat(64);
    meta.put_provenance(
        "hosted",
        "flask",
        &file_digest,
        "flask-1.0.whl",
        crate::store::ProvenanceSibling {
            provenance_sha256: &bundle,
            size: 16,
        },
    )
    .unwrap();

    meta.delete_project_cache("pypi", "flask", &[]).unwrap();

    assert_eq!(
        meta.get_provenance("hosted", "flask", &file_digest, "flask-1.0.whl")
            .unwrap(),
        Some((bundle, 16)),
        "purging a cached project that mirrors the same bytes is not the hosted publication's deletion"
    );
}

#[test]
fn test_delete_project_cache_removes_the_freshness_overlay() {
    let (_dir, meta) = store();
    let record = crate::store::CachedIndex {
        source: None,
        last_modified: None,
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

    meta.delete_project_cache("pypi", "flask", &[]).unwrap();

    assert!(meta.get_driver_value(&freshness_key("pypi/flask")).unwrap().is_none());
}

#[test]
fn test_scan_project_records_visits_each_record() {
    let (_dir, meta) = store();
    meta.put_project("pypi", "flask", "Flask").unwrap();
    let mut seen = Vec::new();
    meta.scan_project_records(|key, value| {
        seen.push((key.to_owned(), value.to_owned()));
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(seen, vec![("pypi/flask".to_owned(), "Flask".to_owned())]);
}

/// The upstream root can still name a project whose detail it answers `404` for. peryx retired the
/// detail, so the root list has to stop naming it too, even while the active catalog generation does.
#[test]
fn test_a_retired_project_leaves_the_root_list_the_catalog_still_names() {
    let (_dir, meta) = store();
    let (id, expected) = begin_catalog_generation(&meta, "pypi").unwrap();
    put_catalog_projects(&meta, "pypi", id, &[("acme".to_owned(), "Acme".to_owned())]).unwrap();
    publish_catalog_generation(&meta, "pypi", expected, generation(id, None, None)).unwrap();
    meta.put_cached_page(crate::store::CachedPageWrite {
        key: "pypi/acme",
        record: &crate::store::CachedIndex {
            source: None,
            last_modified: None,
            etag: None,
            last_serial: None,
            fetched_at_unix: 1,
            content_type: None,
            fresh_secs: None,
            body: Vec::new(),
        },
        index: "pypi",
        normalized: "acme",
        display: "Acme",
        source: "pypi",
        upstream: None,
        project_status: None,
        project_status_reason: None,
        files: &[],
        attestations: &[],
    })
    .unwrap();

    meta.retire_cached_project("pypi/acme", "pypi", "acme").unwrap();

    assert_eq!(meta.list_projects("pypi").unwrap(), Vec::<String>::new());
}
