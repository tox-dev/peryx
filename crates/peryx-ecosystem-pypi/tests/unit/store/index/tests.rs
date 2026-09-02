use std::error::Error as _;
use std::sync::mpsc::sync_channel;
use std::thread;

use super::{CachedIndex, MetaStore, index_key};
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

fn record() -> CachedIndex {
    CachedIndex {
        etag: Some("\"abc\"".to_owned()),
        last_serial: Some(42),
        fetched_at_unix: 1_700_000_000,
        content_type: None,
        fresh_secs: None,
        body: b"<html></html>".to_vec(),
    }
}

#[test]
fn test_put_and_get_index_roundtrip() {
    let (_dir, meta) = store();
    assert_eq!(meta.get_index("root-pypi/flask").unwrap(), None);
    meta.put_index("root-pypi/flask", &record()).unwrap();
    assert_eq!(meta.get_index("root-pypi/flask").unwrap(), Some(record()));
}

#[test]
fn test_put_index_overwrites() {
    let (_dir, meta) = store();
    meta.put_index("k", &record()).unwrap();
    let mut updated = record();
    updated.last_serial = Some(99);
    meta.put_index("k", &updated).unwrap();
    assert_eq!(meta.get_index("k").unwrap().unwrap().last_serial, Some(99));
}

#[test]
fn test_touch_index_freshness_advances_without_rewriting_the_body_row() {
    let (_dir, meta) = store();
    meta.put_index("pypi/flask", &record()).unwrap();
    let body_row = meta.get_driver_value(&index_key("pypi/flask")).unwrap().unwrap();

    meta.touch_index_freshness("pypi/flask", 1_800_000_000, Some(900))
        .unwrap();

    assert_eq!(
        meta.get_driver_value(&index_key("pypi/flask")).unwrap().unwrap(),
        body_row,
        "a 304 rewrites the freshness overlay, not the page body row"
    );
    let refreshed = meta.get_index("pypi/flask").unwrap().unwrap();
    assert_eq!(refreshed.fetched_at_unix, 1_800_000_000);
    assert_eq!(refreshed.fresh_secs, Some(900));
    assert_eq!(refreshed.body, record().body, "the served body is unchanged");
    assert_eq!(refreshed.etag, record().etag);
}

#[test]
fn test_put_index_clears_a_stale_freshness_overlay() {
    let (_dir, meta) = store();
    meta.put_index("k", &record()).unwrap();
    meta.touch_index_freshness("k", 9_999, Some(1)).unwrap();

    let mut replaced = record();
    replaced.fetched_at_unix = 2_000_000_000;
    replaced.body = b"<html>new</html>".to_vec();
    meta.put_index("k", &replaced).unwrap();

    assert_eq!(
        meta.get_index("k").unwrap().unwrap(),
        replaced,
        "a 200 replaces the body and its freshness; the overlay must not shadow it"
    );
}

#[test]
fn test_list_index_pages_reflects_a_freshness_overlay() {
    let (_dir, meta) = store();
    meta.put_index("pypi/flask", &record()).unwrap();
    meta.touch_index_freshness("pypi/flask", 1_900_000_000, Some(120))
        .unwrap();
    assert_eq!(
        meta.list_index_pages().unwrap(),
        vec![("pypi/flask".to_owned(), 1_900_000_000, Some(120))]
    );
}

#[test]
fn test_scan_index_pages_reflects_a_freshness_overlay() {
    let (_dir, meta) = store();
    meta.put_index("pypi/flask", &record()).unwrap();
    meta.touch_index_freshness("pypi/flask", 1_900_000_000, Some(120))
        .unwrap();
    let mut pages = Vec::new();
    meta.scan_index_pages(|page| {
        pages.push((
            page.key,
            page.summary.fetched_at_unix,
            page.summary.fresh_secs,
            page.summary.body_bytes,
        ));
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(pages, vec![("pypi/flask".to_owned(), 1_900_000_000, Some(120), 13)]);
}

#[test]
fn test_put_cached_page_records_file_url_size_and_status() {
    let (_dir, meta) = store();
    meta.put_cached_page(crate::store::CachedPageWrite {
        key: "pypi/pkg",
        record: &record(),
        index: "pypi",
        normalized: "pkg",
        display: "Pkg",
        source: "pypi",
        upstream: Some("mirror"),
        project_status: Some("archived"),
        project_status_reason: Some("read only"),
        files: &[crate::store::PublishedFileWrite {
            sha256: "feedface".to_owned(),
            filename: "pkg-1.0.whl".to_owned(),
            url: "https://files.example/pkg-1.0.whl".to_owned(),
            size: Some(42),
            metadata: Some((
                "https://files.example/pkg-1.0.whl.metadata".to_owned(),
                "decafbad".to_owned(),
            )),
        }],
        attestations: &[],
    })
    .unwrap();

    let source = meta.get_file_url("feedface").unwrap().unwrap();
    assert_eq!(source.size, Some(42), "the file's size line round-trips");
    assert_eq!(source.upstream.as_deref(), Some("mirror"));
    assert_eq!(
        meta.get_file_publication("pypi", "pkg", "feedface", "pkg-1.0.whl")
            .unwrap(),
        Some(crate::store::FilePublication::Claimed(crate::store::MetadataClaim {
            url: "https://files.example/pkg-1.0.whl.metadata".to_owned(),
            metadata_sha256: "decafbad".to_owned(),
            source: "pypi".to_owned(),
            upstream: Some("mirror".to_owned()),
        })),
        "the claim is scoped to the publication that advertised it"
    );
    assert_eq!(
        meta.get_metadata_digest("feedface").unwrap(),
        None,
        "an upstream claim is not metadata peryx derived from the artifact"
    );
    assert_eq!(
        meta.get_project_status("pypi", "pkg")
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("archived")
    );
}

#[test]
fn test_put_cached_page_clears_status_when_none() {
    let (_dir, meta) = store();
    meta.put_cached_page(crate::store::CachedPageWrite {
        key: "pypi/pkg",
        record: &record(),
        index: "pypi",
        normalized: "pkg",
        display: "Pkg",
        source: "pypi",
        upstream: None,
        project_status: None,
        project_status_reason: None,
        files: &[],
        attestations: &[],
    })
    .unwrap();
    assert!(meta.get_project_status("pypi", "pkg").unwrap().is_none());
}

#[test]
fn test_late_page_registration_preserves_or_resets_a_cached_attestation_by_url() {
    let (_dir, meta) = store();
    let filename = "pkg-1.0.whl";
    let first_url = "https://example.test/pkg-1.0.whl.provenance";
    let mut cached = crate::store::UpstreamAttestation::remote(first_url, "pypi", "pkg", Some("primary"));
    cached.media_type = Some("application/json".to_owned());
    cached.etag = Some("\"v1\"".to_owned());
    cached.fetched_at_unix = Some(10);
    cached.availability = crate::store::AttestationAvailability::Cached;
    cached.body = Some("body".to_owned());
    meta.put_upstream_attestation("pypi", "abc", filename, &cached).unwrap();

    meta.put_cached_page(crate::store::CachedPageWrite {
        key: "pypi/pkg",
        record: &record(),
        index: "pypi",
        normalized: "pkg",
        display: "Pkg",
        source: "pypi",
        upstream: Some("primary"),
        project_status: None,
        project_status_reason: None,
        files: &[],
        attestations: &[("abc".to_owned(), filename.to_owned(), first_url.to_owned())],
    })
    .unwrap();

    assert_eq!(
        meta.get_upstream_attestation("pypi", "pkg", "abc", filename).unwrap(),
        Some(cached)
    );

    let second_url = "https://example.test/pkg-1.0.whl.provenance?v=2";
    meta.put_cached_page(crate::store::CachedPageWrite {
        key: "pypi/pkg",
        record: &record(),
        index: "pypi",
        normalized: "pkg",
        display: "Pkg",
        source: "pypi",
        upstream: Some("primary"),
        project_status: None,
        project_status_reason: None,
        files: &[],
        attestations: &[("abc".to_owned(), filename.to_owned(), second_url.to_owned())],
    })
    .unwrap();

    assert_eq!(
        meta.get_upstream_attestation("pypi", "pkg", "abc", filename).unwrap(),
        Some(crate::store::UpstreamAttestation::remote(
            second_url,
            "pypi",
            "pkg",
            Some("primary"),
        ))
    );
}

#[test]
fn test_attestation_compare_exchange_rejects_a_source_identity_change() {
    let (_dir, meta) = store();
    let expected = crate::store::UpstreamAttestation::remote(
        "https://example.test/pkg-1.0.whl.provenance",
        "pypi",
        "pkg",
        Some("primary"),
    );
    meta.put_upstream_attestation("pypi", "abc", "pkg-1.0.whl", &expected)
        .unwrap();
    let mut replacement = expected.clone();
    replacement.source = "other".to_owned();

    let error = meta
        .compare_exchange_upstream_attestation("pypi", "abc", "pkg-1.0.whl", &expected, &replacement)
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "driver precondition failed: attestation cache replacement changed its source identity"
    );
    assert_eq!(
        meta.get_upstream_attestation("pypi", "pkg", "abc", "pkg-1.0.whl")
            .unwrap(),
        Some(expected)
    );
}

#[test]
fn test_list_index_pages_reports_freshness() {
    let (_dir, meta) = store();
    meta.put_index("pypi/flask", &record()).unwrap();
    meta.put_index(
        "pypi/numpy",
        &CachedIndex {
            fetched_at_unix: 1_800_000_000,
            fresh_secs: Some(600),
            ..record()
        },
    )
    .unwrap();
    let mut pages = meta.list_index_pages().unwrap();
    pages.sort();
    assert_eq!(
        pages,
        vec![
            ("pypi/flask".to_owned(), 1_700_000_000, None),
            ("pypi/numpy".to_owned(), 1_800_000_000, Some(600)),
        ]
    );
}

#[test]
fn test_list_index_pages_reads_a_legacy_plain_json_record() {
    let (_dir, meta) = store();

    let legacy = serde_json::to_vec(&record()).unwrap();
    meta.put_driver_value(&index_key("pypi/old"), &legacy).unwrap();
    assert_eq!(
        meta.list_index_pages().unwrap(),
        vec![("pypi/old".to_owned(), 1_700_000_000, None)]
    );
}

#[test]
fn test_list_index_pages_rejects_a_malformed_record() {
    let (_dir, meta) = store();
    meta.put_driver_value(&index_key("a"), b"not an index").unwrap();
    meta.put_index("z", &record()).unwrap();

    assert!(meta.list_index_pages().is_err());
}

#[test]
fn test_scan_index_pages_visits_records_without_collecting() {
    let (_dir, meta) = store();
    meta.put_index("pypi/flask", &record()).unwrap();
    let mut pages = Vec::new();
    meta.scan_index_pages(|page| {
        pages.push((page.key, page.summary.body_bytes));
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(pages, vec![("pypi/flask".to_owned(), 13)]);
}

#[test]
fn test_scan_index_pages_reports_the_visitor_error_source() {
    let (_dir, meta) = store();
    meta.put_index("pypi/flask", &record()).unwrap();
    let err = meta
        .scan_index_pages(|_page| Err(std::io::Error::other("stop")))
        .unwrap_err();
    assert_eq!(err.to_string(), "stop");
    assert!(err.source().is_some());
}

#[test]
fn test_scan_index_pages_rejects_a_malformed_record() {
    let (_dir, meta) = store();
    meta.put_index("a", &record()).unwrap();
    meta.put_driver_value(&index_key("m"), b"not an index").unwrap();
    meta.put_index("z", &record()).unwrap();
    let mut visited = 0;

    let error = meta
        .scan_index_pages(|_page| {
            visited += 1;
            Ok::<(), std::convert::Infallible>(())
        })
        .unwrap_err();

    assert_eq!(visited, 1);
    assert!(matches!(error, peryx_storage::meta::MetaScanError::Store(_)));
}

#[test]
fn test_list_index_pages_reports_a_missing_driver_table() {
    let (_directory, meta) = uninitialized_store();

    assert!(meta.list_index_pages().is_err());
}

#[test]
fn test_scan_index_pages_propagates_store_errors_after_visiting_healthy_records() {
    let (_valid_directory, valid) = store();
    valid.put_index("a", &record()).unwrap();
    let (_invalid_directory, invalid) = uninitialized_store();
    let mut seen = 0;
    let mut visit = |_page| {
        seen += 1;
        Ok::<(), std::convert::Infallible>(())
    };
    valid.scan_index_pages(&mut visit).unwrap();

    let error = invalid.scan_index_pages(&mut visit).unwrap_err();

    assert_eq!(seen, 1);
    assert!(matches!(error, peryx_storage::meta::MetaScanError::Store(_)));
}

#[test]
fn test_scan_index_records_propagates_store_errors_after_visiting_healthy_records() {
    let (_valid_directory, valid) = store();
    valid.put_index("a", &record()).unwrap();
    let (_invalid_directory, invalid) = uninitialized_store();
    let mut seen = 0;
    let mut visit = |_key: &str, _value: &[u8]| {
        seen += 1;
        Ok::<(), std::convert::Infallible>(())
    };
    valid.scan_index_records(&mut visit).unwrap();

    let error = invalid.scan_index_records(&mut visit).unwrap_err();

    assert_eq!(seen, 1);
    assert!(matches!(error, peryx_storage::meta::MetaScanError::Store(_)));
}

#[test]
fn test_list_project_files_reports_a_missing_driver_table() {
    let (_directory, meta) = uninitialized_store();

    assert!(super::list_project_files(&meta, "pypi", "flask").is_err());
}

#[test]
fn test_scan_index_records_visits_raw_bytes() {
    let (_dir, meta) = store();
    meta.put_index("pypi/flask", &record()).unwrap();
    let mut keys = Vec::new();
    meta.scan_index_records(|key, raw| {
        keys.push((key.to_owned(), raw.starts_with(b"peryx1\n")));
        Ok::<(), std::io::Error>(())
    })
    .unwrap();
    assert_eq!(keys, vec![("pypi/flask".to_owned(), true)]);
}

#[test]
fn test_scan_index_records_keeps_one_snapshot_during_refresh() {
    let (_dir, meta) = store();
    meta.put_index("pypi/a", &record()).unwrap();
    let mut original = record();
    original.last_serial = Some(7);
    meta.put_index("pypi/z", &original).unwrap();
    let original_raw = meta.get_driver_value(&index_key("pypi/z")).unwrap().unwrap();
    let mut replacement = original;
    replacement.last_serial = Some(8);
    let persisted = replacement.clone();
    let (scan_started_tx, scan_started_rx) = sync_channel(0);
    let (refresh_done_tx, refresh_done_rx) = sync_channel(0);
    let mut scanned = None;

    thread::scope(|scope| {
        let refresh_meta = &meta;
        scope.spawn(move || {
            scan_started_rx.recv().unwrap();
            refresh_meta.put_index("pypi/z", &replacement).unwrap();
            refresh_done_tx.send(()).unwrap();
        });
        meta.scan_index_records(|key, raw| {
            if key == "pypi/a" {
                scan_started_tx.send(()).unwrap();
                refresh_done_rx.recv().unwrap();
            } else {
                scanned = Some(raw.to_vec());
            }
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
    });

    assert_eq!(scanned, Some(original_raw));
    assert_eq!(meta.get_index("pypi/z").unwrap(), Some(persisted));
}

mod generation {
    use std::collections::BTreeMap;

    use super::super::{
        abort_project_generation, active_project_generation, begin_project_generation, list_project_files,
        project_files_in_snapshot, project_generation_prefix, project_meta_state, publish_project_generation,
        put_project_files, recover_project_generations, refresh_project_generation,
    };
    use super::MetaStore;
    use crate::simple::{CoreMetadata, File, Provenance, Yanked};
    use crate::store::{ProjectGeneration, PypiStore as _};

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    fn file(filename: &str, sha256: Option<&str>) -> File {
        File {
            filename: filename.to_owned(),
            url: format!("https://files.example/{filename}"),
            hashes: sha256
                .map(|digest| BTreeMap::from([("sha256".to_owned(), digest.to_owned())]))
                .unwrap_or_default(),
            requires_python: None,
            size: Some(10),
            upload_time: Some("2024-01-01T00:00:00Z".to_owned()),
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: crate::simple::Provenance::Absent,
        }
    }

    fn attested_file(filename: &str, sha256: &str) -> File {
        let mut file = file(filename, Some(sha256));
        file.provenance = Provenance::Url(format!("https://files.example/{filename}.provenance"));
        file
    }

    fn generation(id: u64, etag: Option<&str>, files: u64) -> ProjectGeneration {
        ProjectGeneration {
            generation: id,
            source: "pypi".to_owned(),
            url: "https://pypi.org/simple/flask/".to_owned(),
            format: "json".to_owned(),
            etag: etag.map(str::to_owned),
            last_modified: None,
            last_serial: Some(3),
            fetched_at_unix: 1,
            bytes: 100,
            files,
            versions: vec!["1.0".to_owned()],
            project_status: None,
            project_status_reason: None,
        }
    }

    fn publish(meta: &MetaStore, index: &str, project: &str, files: &[File]) -> u64 {
        let (id, expected) = begin_project_generation(meta, index, project).unwrap();
        let admitted = put_project_files(meta, index, project, id, "pypi", None, files).unwrap();
        publish_project_generation(meta, index, project, expected, generation(id, Some("etag"), admitted)).unwrap();
        id
    }

    #[test]
    fn test_publish_lists_files_and_registers_download_rows() {
        let (_dir, meta) = store();
        let mut wheel = file("flask-1.0-py3-none-any.whl", Some(&"a".repeat(64)));
        wheel.set_metadata(CoreMetadata::Hashes(BTreeMap::from([(
            "sha256".to_owned(),
            "b".repeat(64),
        )])));

        publish(
            &meta,
            "pypi",
            "flask",
            &[wheel.clone(), file("flask-1.0.tar.gz", Some(&"c".repeat(64)))],
        );

        let listed = list_project_files(&meta, "pypi", "flask").unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].filename, "flask-1.0-py3-none-any.whl");
        let source = meta.get_file_url(&"a".repeat(64)).unwrap().unwrap();
        assert_eq!(source.url, "https://files.example/flask-1.0-py3-none-any.whl");
        assert_eq!(source.size, Some(10));
        let publication = meta
            .get_file_publication("pypi", "flask", &"a".repeat(64), "flask-1.0-py3-none-any.whl")
            .unwrap();
        assert_eq!(
            publication,
            Some(crate::store::FilePublication::Claimed(crate::store::MetadataClaim {
                url: "https://files.example/flask-1.0-py3-none-any.whl.metadata".to_owned(),
                metadata_sha256: "b".repeat(64),
                source: "pypi".to_owned(),
                upstream: None,
            }))
        );
    }

    #[test]
    fn test_active_generation_records_counts_and_validators() {
        let (_dir, meta) = store();
        let id = publish(
            &meta,
            "pypi",
            "flask",
            &[file("flask-1.0.tar.gz", Some(&"a".repeat(64)))],
        );
        let active = active_project_generation(&meta, "pypi", "flask").unwrap().unwrap();
        assert_eq!(active.generation, id);
        assert_eq!(active.files, 1);
        assert_eq!(active.etag.as_deref(), Some("etag"));
    }

    #[test]
    fn test_put_project_files_is_first_wins_and_counts_new_filenames() {
        let (_dir, meta) = store();
        let (id, _) = begin_project_generation(&meta, "pypi", "flask").unwrap();
        let first = file("flask-1.0.tar.gz", Some(&"a".repeat(64)));
        let again = file("flask-1.0.tar.gz", Some(&"d".repeat(64)));
        assert_eq!(
            put_project_files(&meta, "pypi", "flask", id, "pypi", None, &[first, again]).unwrap(),
            1
        );

        assert_eq!(
            put_project_files(
                &meta,
                "pypi",
                "flask",
                id,
                "pypi",
                None,
                &[file("flask-2.0.tar.gz", Some(&"e".repeat(64)))]
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn test_put_project_files_stores_a_file_without_a_hash_but_registers_no_source() {
        let (_dir, meta) = store();
        let (id, expected) = begin_project_generation(&meta, "pypi", "flask").unwrap();
        put_project_files(
            &meta,
            "pypi",
            "flask",
            id,
            "pypi",
            None,
            &[file("flask-1.0.tar.gz", None)],
        )
        .unwrap();
        publish_project_generation(&meta, "pypi", "flask", expected, generation(id, None, 1)).unwrap();
        assert_eq!(list_project_files(&meta, "pypi", "flask").unwrap().len(), 1);
    }

    #[test]
    fn test_put_project_files_requires_its_staging_generation() {
        let (_dir, meta) = store();
        let error = put_project_files(
            &meta,
            "pypi",
            "flask",
            7,
            "pypi",
            None,
            &[file("f.tar.gz", Some(&"a".repeat(64)))],
        )
        .unwrap_err();
        assert!(matches!(error, peryx_storage::meta::MetaError::DriverPrecondition(_)));
    }

    #[test]
    fn test_publish_lost_reservation_is_rejected() {
        let (_dir, meta) = store();
        let (first, expected) = begin_project_generation(&meta, "pypi", "flask").unwrap();
        begin_project_generation(&meta, "pypi", "flask").unwrap();
        assert!(publish_project_generation(&meta, "pypi", "flask", expected, generation(first, None, 0)).is_err());
    }

    #[test]
    fn test_list_files_is_empty_without_an_active_generation() {
        let (_dir, meta) = store();
        assert!(list_project_files(&meta, "pypi", "flask").unwrap().is_empty());
        assert!(active_project_generation(&meta, "pypi", "flask").unwrap().is_none());
    }

    #[test]
    fn test_list_files_keeps_one_generation_when_publication_reclaims_mid_read() {
        let (_dir, meta) = store();
        let retired = [
            file("flask-1.0.tar.gz", Some(&"a".repeat(64))),
            file("flask-1.1.tar.gz", Some(&"b".repeat(64))),
        ];
        let replacement = file("flask-2.0.tar.gz", Some(&"c".repeat(64)));
        publish(&meta, "pypi", "flask", &retired);

        let during = meta
            .read_driver_txn(|txn| {
                publish(&meta, "pypi", "flask", std::slice::from_ref(&replacement));
                recover_project_generations(&meta, "pypi", "flask").unwrap();
                project_files_in_snapshot(txn, "pypi", "flask")
            })
            .unwrap();

        assert_eq!(during, retired);
        assert_eq!(list_project_files(&meta, "pypi", "flask").unwrap(), vec![replacement]);
    }

    #[test]
    fn test_list_files_reports_a_malformed_row() {
        let (_dir, meta) = store();
        let id = publish(
            &meta,
            "pypi",
            "flask",
            &[file("flask-1.0.tar.gz", Some(&"a".repeat(64)))],
        );
        meta.put_driver_value(
            &super::super::project_file_key("pypi", "flask", id, "flask-1.0.tar.gz"),
            b"not a file record",
        )
        .unwrap();
        meta.put_driver_value(
            &super::super::project_file_key("pypi", "flask", id, "z.tar.gz"),
            &serde_json::to_vec(&file("z.tar.gz", None)).unwrap(),
        )
        .unwrap();
        assert!(list_project_files(&meta, "pypi", "flask").is_err());
    }

    #[test]
    fn test_abort_removes_only_its_generation_rows() {
        let (_dir, meta) = store();
        let published = publish(
            &meta,
            "pypi",
            "flask",
            &[file("flask-1.0.tar.gz", Some(&"a".repeat(64)))],
        );
        let (staging, _) = begin_project_generation(&meta, "pypi", "flask").unwrap();
        put_project_files(
            &meta,
            "pypi",
            "flask",
            staging,
            "pypi",
            None,
            &[file("flask-2.0.tar.gz", Some(&"b".repeat(64)))],
        )
        .unwrap();

        abort_project_generation(&meta, "pypi", "flask", staging).unwrap();

        let state = project_meta_state(&meta, "pypi", "flask").unwrap();
        assert_eq!(state.active.unwrap().generation, published);
        assert!(state.staging.is_none());
        assert!(
            meta.driver_prefix_keys(&project_generation_prefix("pypi", "flask", staging))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_staged_attestation_is_invisible_and_abort_removes_it() {
        let (_dir, meta) = store();
        let digest = "b".repeat(64);
        let filename = "flask-2.0.tar.gz";
        let (staging, _) = begin_project_generation(&meta, "pypi", "flask").unwrap();
        put_project_files(
            &meta,
            "pypi",
            "flask",
            staging,
            "pypi",
            None,
            &[attested_file(filename, &digest)],
        )
        .unwrap();

        assert!(
            meta.list_upstream_attestations("pypi", &digest, filename)
                .unwrap()
                .is_empty()
        );

        abort_project_generation(&meta, "pypi", "flask", staging).unwrap();

        assert!(
            meta.list_upstream_attestations("pypi", &digest, filename)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_new_generation_retires_removed_attestation_locator() {
        let (_dir, meta) = store();
        let digest = "b".repeat(64);
        let filename = "flask-1.0.tar.gz";
        publish(&meta, "pypi", "flask", &[attested_file(filename, &digest)]);
        assert_eq!(
            meta.list_upstream_attestations("pypi", &digest, filename)
                .unwrap()
                .len(),
            1
        );

        publish(&meta, "pypi", "flask", &[file(filename, Some(&digest))]);

        assert!(
            meta.list_upstream_attestations("pypi", &digest, filename)
                .unwrap()
                .is_empty()
        );
        assert!(
            meta.get_upstream_attestation("pypi", "flask", &digest, filename)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_abort_leaves_a_newer_staging_reservation() {
        let (_dir, meta) = store();
        let (first, _) = begin_project_generation(&meta, "pypi", "flask").unwrap();
        let (second, _) = begin_project_generation(&meta, "pypi", "flask").unwrap();
        abort_project_generation(&meta, "pypi", "flask", first).unwrap();
        assert_eq!(
            project_meta_state(&meta, "pypi", "flask").unwrap().staging,
            Some(second)
        );
    }

    #[test]
    fn test_refresh_merges_present_validators_and_advances_time() {
        let (_dir, meta) = store();
        let id = publish(
            &meta,
            "pypi",
            "flask",
            &[file("flask-1.0.tar.gz", Some(&"a".repeat(64)))],
        );
        refresh_project_generation(&meta, "pypi", "flask", id, None, Some("mon".to_owned()), 99).unwrap();
        assert!(refresh_project_generation(&meta, "pypi", "flask", id + 1, None, None, 100).is_err());
        let active = active_project_generation(&meta, "pypi", "flask").unwrap().unwrap();
        assert_eq!(active.etag.as_deref(), Some("etag"));
        assert_eq!(active.last_modified.as_deref(), Some("mon"));
        assert_eq!(active.fetched_at_unix, 99);
    }

    #[test]
    fn test_recover_preserves_active_and_sweeps_pending_generations() {
        let (_dir, meta) = store();
        let active = publish(
            &meta,
            "pypi",
            "flask",
            &[file("flask-1.0.tar.gz", Some(&"a".repeat(64)))],
        );
        let (staging, _) = begin_project_generation(&meta, "pypi", "flask").unwrap();
        put_project_files(
            &meta,
            "pypi",
            "flask",
            staging,
            "pypi",
            None,
            &[file("flask-2.0.tar.gz", Some(&"b".repeat(64)))],
        )
        .unwrap();

        recover_project_generations(&meta, "pypi", "flask").unwrap();

        let state = project_meta_state(&meta, "pypi", "flask").unwrap();
        assert_eq!(state.active.unwrap().generation, active);
        assert!(state.staging.is_none());
        assert!(state.retired.is_none());
        assert!(
            meta.driver_prefix_keys(&project_generation_prefix("pypi", "flask", staging))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn test_retire_cached_project_drops_its_publication_records() {
    let (_dir, meta) = store();
    meta.put_cached_page(crate::store::CachedPageWrite {
        key: "pypi/pkg",
        record: &record(),
        index: "pypi",
        normalized: "pkg",
        display: "Pkg",
        source: "pypi",
        upstream: None,
        project_status: None,
        project_status_reason: None,
        files: &[crate::store::PublishedFileWrite {
            sha256: "feedface".to_owned(),
            filename: "pkg-1.0.whl".to_owned(),
            url: "https://files.example/pkg-1.0.whl".to_owned(),
            size: None,
            metadata: Some((
                "https://files.example/pkg-1.0.whl.metadata".to_owned(),
                "decafbad".to_owned(),
            )),
        }],
        attestations: &[],
    })
    .unwrap();

    meta.retire_cached_project("pypi/pkg", "pypi", "pkg").unwrap();

    assert_eq!(
        meta.get_file_publication("pypi", "pkg", "feedface", "pkg-1.0.whl")
            .unwrap(),
        None
    );
}
