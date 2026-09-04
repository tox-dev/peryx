//! Every corruption here is written straight into the store rather than provoked through a write
//! path, because the write paths are what maintain these rows: the states the audit exists to catch
//! are exactly the ones no write path can reach.

use peryx_storage::meta::MetaStore;

use crate::store::{AuditedIndex, PypiStore as _, SummaryDefect, audit_summary_rows, repair_summary_rows};

/// The `hosted` index's count row, and the order row of the one upload the fixtures publish.
const COUNT_KEY: &str = "pypi\u{0}k\u{0}hosted";
const ORDER_KEY: &str = "pypi\u{0}w\u{0}hosted\u{0}009223372035087550207/flask-1.0.whl\u{0}flask\u{0}flask-1.0.whl";
const ORDER_VALUE: &str =
    r#"{"resource":"flask","artifact":"flask-1.0.whl","group":"1.0","written_at":"2026-01-01T00:00:00Z","size":10}"#;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

/// One published upload on `hosted`, written the way the serving paths write it, so the derived rows
/// start out correct and each test damages exactly one of them.
fn published() -> (tempfile::TempDir, MetaStore) {
    let (dir, meta) = store();
    meta.put_upload(
        "hosted",
        "flask",
        "flask-1.0.whl",
        br#"{"version":"1.0","file":{"filename":"flask-1.0.whl","upload-time":"2026-01-01T00:00:00Z","size":10}}"#,
    )
    .unwrap();
    (dir, meta)
}

fn hosted() -> Vec<AuditedIndex<'static>> {
    vec![AuditedIndex {
        name: "hosted",
        local: false,
    }]
}

fn cached() -> Vec<AuditedIndex<'static>> {
    vec![AuditedIndex {
        name: "pypi",
        local: true,
    }]
}

fn audit(meta: &MetaStore, indexes: &[AuditedIndex<'_>]) -> Vec<SummaryDefect> {
    audit_summary_rows(meta, indexes).unwrap()
}

fn messages(defects: &[SummaryDefect]) -> Vec<(&str, &str)> {
    defects
        .iter()
        .map(|defect| (defect.namespace, defect.message.as_str()))
        .collect()
}

#[test]
fn test_audit_passes_a_store_whose_rows_were_all_written_through_the_write_path() {
    let (_dir, meta) = published();
    meta.put_project("hosted", "flask", "Flask").unwrap();

    assert_eq!(audit(&meta, &hosted()), Vec::new());
}

#[test]
fn test_audit_passes_an_index_with_no_rows_at_all() {
    let (_dir, meta) = store();

    assert_eq!(audit(&meta, &hosted()), Vec::new());
}

#[test]
fn test_audit_reports_a_count_row_that_overstates_its_uploads() {
    let (_dir, meta) = published();
    meta.put_driver_value(COUNT_KEY, b"0\n4").unwrap();

    assert_eq!(
        messages(&audit(&meta, &hosted())),
        vec![(
            "summary-count",
            "count row says 0 projects and 4 uploads, rows hold 0 projects and 1 uploads"
        )]
    );
}

#[test]
fn test_audit_reports_a_count_row_that_no_rows_account_for() {
    let (_dir, meta) = store();
    meta.put_driver_value(COUNT_KEY, b"3\n3").unwrap();

    assert_eq!(
        messages(&audit(&meta, &hosted())),
        vec![(
            "summary-count",
            "count row says 3 projects and 3 uploads, rows hold none"
        )]
    );
}

#[test]
fn test_audit_reports_an_absent_count_row_for_rows_that_exist() {
    let (_dir, meta) = published();
    meta.delete_driver_value(COUNT_KEY).unwrap();

    assert_eq!(
        messages(&audit(&meta, &hosted())),
        vec![(
            "summary-count",
            "count row is absent, rows hold 0 projects and 1 uploads"
        )]
    );
}

#[test]
fn test_audit_reports_a_count_row_that_does_not_decode() {
    let (_dir, meta) = published();
    meta.put_driver_value(COUNT_KEY, b"many\n1").unwrap();

    assert_eq!(
        messages(&audit(&meta, &hosted())),
        vec![(
            "summary-count",
            "driver record \"pypi\\0k\\0hosted\" has invalid integer field \"projects\""
        )]
    );
}

#[test]
fn test_audit_reports_an_order_row_no_upload_accounts_for() {
    let (_dir, meta) = published();
    meta.put_driver_value(
        "pypi\u{0}w\u{0}hosted\u{0}009223372035087550207/ghost.whl\u{0}flask\u{0}ghost.whl",
        ORDER_VALUE.as_bytes(),
    )
    .unwrap();

    assert_eq!(
        messages(&audit(&meta, &hosted())),
        vec![("summary-order", "order row has no upload")]
    );
}

#[test]
fn test_audit_reports_an_upload_with_no_order_row() {
    let (_dir, meta) = published();
    meta.delete_driver_value(ORDER_KEY).unwrap();

    assert_eq!(
        messages(&audit(&meta, &hosted())),
        vec![("summary-order", "upload hosted/flask/flask-1.0.whl has no order row")]
    );
}

#[test]
fn test_audit_reports_an_order_row_whose_fields_no_longer_match_its_upload() {
    let (_dir, meta) = published();
    meta.put_driver_value(
        ORDER_KEY,
        br#"{"resource":"flask","artifact":"flask-1.0.whl","group":"9.9","written_at":"2026-01-01T00:00:00Z","size":10}"#,
    )
    .unwrap();

    assert_eq!(
        messages(&audit(&meta, &hosted())),
        vec![(
            "summary-order",
            "order row does not match upload hosted/flask/flask-1.0.whl"
        )]
    );
}

#[test]
fn test_audit_reports_an_order_row_that_does_not_decode() {
    let (_dir, meta) = published();
    meta.put_driver_value(ORDER_KEY, b"not json").unwrap();

    assert_eq!(
        messages(&audit(&meta, &hosted()))
            .first()
            .map(|(namespace, _)| *namespace),
        Some("summary-order")
    );
}

/// A virtual index owns no projects and no uploads, so a derived row naming one is a row that should
/// not exist rather than a count that disagrees.
#[test]
fn test_audit_reports_a_derived_row_for_an_index_that_owns_none() {
    let (_dir, meta) = store();
    meta.put_driver_value("pypi\u{0}k\u{0}virtual", b"1\n1").unwrap();
    meta.put_driver_value("pypi\u{0}w\u{0}virtual\u{0}position", ORDER_VALUE.as_bytes())
        .unwrap();

    assert_eq!(
        messages(&audit(&meta, &hosted())),
        vec![
            ("summary-count", "no cached or hosted index owns this row"),
            ("summary-order", "no cached or hosted index owns this row"),
        ]
    );
}

/// An order key carries an index name terminated by NUL. A row with no terminator names no index.
#[test]
fn test_audit_reports_an_order_row_whose_key_names_no_index() {
    let (_dir, meta) = store();
    meta.put_driver_value("pypi\u{0}w\u{0}hosted", ORDER_VALUE.as_bytes())
        .unwrap();

    assert_eq!(
        messages(&audit(&meta, &hosted())),
        vec![("summary-order", "no cached or hosted index owns this row")]
    );
}

/// A record that is not JSON has no fields to report, so the write path gives it no order row. Reading
/// that absence as a defect would fail the audit of every store holding one damaged upload.
#[test]
fn test_audit_expects_no_order_row_for_an_upload_that_is_not_json() {
    let (_dir, meta) = store();
    meta.put_upload("hosted", "flask", "flask-1.0.whl", b"not json")
        .unwrap();

    assert_eq!(audit(&meta, &hosted()), Vec::new());
}

/// A count row goes away with the last row it counted, so an absent row and a pair of zeroes are the
/// same state and only the absent one is correct.
#[test]
fn test_audit_reports_a_count_row_of_zeroes_that_should_have_been_removed() {
    let (_dir, meta) = store();
    meta.put_driver_value(COUNT_KEY, b"0\n0").unwrap();

    assert_eq!(
        messages(&audit(&meta, &hosted())),
        vec![(
            "summary-count",
            "count row says 0 projects and 0 uploads, rows hold none"
        )]
    );
}

/// `root` and `root/hosted` are separate indexes whose project and upload keys share a leading
/// segment, so the shorter name must not count the longer one's rows.
#[test]
fn test_audit_counts_a_name_prefixed_index_against_its_own_rows() {
    let (_dir, meta) = store();
    meta.put_project("root", "flask", "Flask").unwrap();
    meta.put_project("root/hosted", "django", "Django").unwrap();
    let indexes = vec![
        AuditedIndex {
            name: "root",
            local: false,
        },
        AuditedIndex {
            name: "root/hosted",
            local: false,
        },
    ];

    assert_eq!(audit(&meta, &indexes), Vec::new());
}

/// A cached index's project markers are node-local, and so is the count row that follows them. The
/// audit compares a store against itself, so the discipline changes how a repair writes rather than
/// what counts as a defect.
#[test]
fn test_audit_holds_a_cached_index_to_its_own_rows() {
    let (_dir, meta) = store();
    meta.put_driver_value("pypi\u{0}p\u{0}pypi/flask", b"Flask").unwrap();

    assert_eq!(
        messages(&audit(&meta, &cached())),
        vec![(
            "summary-count",
            "count row is absent, rows hold 1 projects and 0 uploads"
        )]
    );
}

#[test]
fn test_repair_leaves_a_healthy_store_untouched() {
    let (_dir, meta) = published();

    assert_eq!(repair_summary_rows(&meta, &hosted()).unwrap(), Vec::new());
}

/// Each repair rebuilds from the rows it summarizes, so a re-run of the audit finds nothing.
#[rstest::rstest]
#[case::overstated_count(COUNT_KEY, Some(b"9\n9".as_slice()))]
#[case::absent_count(COUNT_KEY, None)]
#[case::undecodable_count(COUNT_KEY, Some(b"many\n1".as_slice()))]
#[case::missing_order(ORDER_KEY, None)]
#[case::undecodable_order(ORDER_KEY, Some(b"not json".as_slice()))]
#[case::stale_order(
    ORDER_KEY,
    Some(br#"{"resource":"flask","artifact":"flask-1.0.whl","group":"9.9","written_at":null,"size":null}"#.as_slice())
)]
fn test_repair_rebuilds_a_damaged_row_and_the_audit_then_passes(#[case] key: &str, #[case] value: Option<&[u8]>) {
    let (_dir, meta) = published();
    match value {
        Some(value) => meta.put_driver_value(key, value).unwrap(),
        None => {
            meta.delete_driver_value(key).unwrap();
        }
    }

    let repaired = repair_summary_rows(&meta, &hosted()).unwrap();

    assert_eq!(repaired.len(), 1);
    assert_eq!(audit(&meta, &hosted()), Vec::new());
}

#[test]
fn test_repair_removes_an_order_row_no_upload_accounts_for() {
    let (_dir, meta) = published();
    let ghost = "pypi\u{0}w\u{0}hosted\u{0}009223372035087550207/ghost.whl\u{0}flask\u{0}ghost.whl";
    meta.put_driver_value(ghost, ORDER_VALUE.as_bytes()).unwrap();

    let repaired = repair_summary_rows(&meta, &hosted()).unwrap();

    assert_eq!(repaired.len(), 1);
    assert_eq!(meta.get_driver_value(ghost).unwrap(), None);
    assert_eq!(audit(&meta, &hosted()), Vec::new());
}

#[test]
fn test_repair_removes_a_derived_row_for_an_index_that_owns_none() {
    let (_dir, meta) = store();
    meta.put_driver_value("pypi\u{0}k\u{0}virtual", b"1\n1").unwrap();

    let repaired = repair_summary_rows(&meta, &hosted()).unwrap();

    assert_eq!(repaired.len(), 1);
    assert_eq!(meta.get_driver_value("pypi\u{0}k\u{0}virtual").unwrap(), None);
}

/// A drained index's count row is removed rather than written as a pair of zeroes.
#[test]
fn test_repair_removes_a_count_row_that_counts_nothing() {
    let (_dir, meta) = store();
    meta.put_driver_value(COUNT_KEY, b"0\n0").unwrap();

    assert_eq!(repair_summary_rows(&meta, &hosted()).unwrap().len(), 1);
    assert_eq!(meta.get_driver_value(COUNT_KEY).unwrap(), None);
}

/// The repaired summary is the one a reader gets, not just rows that pass a re-audit.
#[test]
fn test_repair_restores_the_summary_a_reader_sees() {
    let (_dir, meta) = published();
    meta.put_driver_value(COUNT_KEY, b"0\n7").unwrap();
    meta.delete_driver_value(ORDER_KEY).unwrap();

    repair_summary_rows(&meta, &hosted()).unwrap();

    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    assert_eq!(summary["hosted"].write_count, 1);
    assert_eq!(
        summary["hosted"]
            .recent_writes
            .iter()
            .map(|write| write.artifact.clone())
            .collect::<Vec<_>>(),
        vec!["flask-1.0.whl"]
    );
}

/// An upload key the write path could not have produced is damage the key checks already name.
/// Counting it here would report the same row a second time and then invent an order row for it.
#[rstest::rstest]
#[case::no_filename("pypi\u{0}u\u{0}hosted/flask/")]
#[case::no_project("pypi\u{0}u\u{0}hosted//flask-1.0.whl")]
fn test_audit_counts_no_upload_whose_key_the_write_path_could_not_produce(#[case] key: &str) {
    let (_dir, meta) = store();
    meta.put_driver_value(key, br#"{"file":{"filename":"flask-1.0.whl"}}"#)
        .unwrap();

    assert_eq!(audit(&meta, &hosted()), Vec::new());
}

#[test]
fn test_audit_counts_no_project_whose_key_names_no_project() {
    let (_dir, meta) = store();
    meta.put_driver_value("pypi\u{0}p\u{0}hosted/", b"Flask").unwrap();

    assert_eq!(audit(&meta, &hosted()), Vec::new());
}

/// The count is assembled from two scans, so a failure in either must not come back as a smaller
/// count: a partial total reads as a real answer and an operator has no way to tell it apart from a
/// store that genuinely holds fewer rows. Every injection point returns the whole count or an error.
///
/// A store handle does not survive its own injected failure, so each step reopens the retained pages
/// rather than reusing one handle.
#[test]
fn summary_row_counts_never_returns_a_partial_count() {
    let (pages, fault) = peryx_test_support::fault::backend();
    let meta = MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    meta.put_upload(
        "hosted",
        "flask",
        "flask-1.0.whl",
        br#"{"version":"1.0","file":{"filename":"flask-1.0.whl","upload-time":"2026-01-01T00:00:00Z","size":10}}"#,
    )
    .unwrap();
    let clean = crate::store::summary_row_counts(&meta).unwrap();
    assert_ne!(clean, crate::store::SummaryRowCounts::default());
    drop(meta);

    let mut failed = 0_u32;
    for fail_after in 0..128 {
        let meta = MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        fault.arm(fail_after);
        let counted = crate::store::summary_row_counts(&meta);
        fault.disable();
        match counted {
            Ok(counts) => assert_eq!(counts, clean, "injecting after {fail_after} reads counted short"),
            Err(_) => failed += 1,
        }
    }

    assert!(failed > 0, "no injection point reached either scan");
}

/// The defect list is assembled from four scans, so a failure in any of them must not come back as a
/// shorter list. A short list of defects is a false all-clear in the same way a short count is a
/// wrong answer: an operator reads "two defects" as the whole truth and stops looking.
///
/// A store handle does not survive its own injected failure, so each step reopens the retained pages
/// rather than reusing one handle.
#[test]
fn audit_summary_rows_never_returns_a_partial_defect_list() {
    let (pages, fault) = peryx_test_support::fault::backend();
    let meta = MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    meta.put_upload(
        "hosted",
        "flask",
        "flask-1.0.whl",
        br#"{"version":"1.0","file":{"filename":"flask-1.0.whl","upload-time":"2026-01-01T00:00:00Z","size":10}}"#,
    )
    .unwrap();
    meta.put_driver_value(COUNT_KEY, b"0\n4").unwrap();
    meta.put_driver_value(ORDER_KEY, b"not json").unwrap();
    let clean = audit_summary_rows(&meta, &hosted()).unwrap();
    assert!(clean.len() > 1, "{clean:?}");
    drop(meta);

    let mut failed = 0_u32;
    for fail_after in 0..192 {
        let meta = MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        fault.arm(fail_after);
        let audited = audit_summary_rows(&meta, &hosted());
        fault.disable();
        match audited {
            Ok(defects) => assert_eq!(defects, clean, "injecting after {fail_after} reads audited short"),
            Err(_) => failed += 1,
        }
    }

    assert!(failed > 0, "no injection point reached the scans");
}

/// A repair that cannot finish its write must leave the store exactly as it found it. The shape to
/// rule out is a half-repaired store: some rows rebuilt and some not, which a later audit reports as
/// a smaller set of defects and an operator reads as progress rather than a failed repair.
///
/// A store handle does not survive its own injected failure, so each step reopens the retained pages
/// rather than reusing one handle.
#[test]
fn repair_summary_rows_leaves_no_half_repaired_store() {
    let (pages, fault) = peryx_test_support::fault::backend();
    let meta = MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    meta.put_upload(
        "hosted",
        "flask",
        "flask-1.0.whl",
        br#"{"version":"1.0","file":{"filename":"flask-1.0.whl","upload-time":"2026-01-01T00:00:00Z","size":10}}"#,
    )
    .unwrap();
    meta.put_driver_value(COUNT_KEY, b"0\n4").unwrap();
    meta.put_driver_value(ORDER_KEY, b"not json").unwrap();
    let damaged = audit_summary_rows(&meta, &hosted()).unwrap();
    assert!(damaged.len() > 1, "{damaged:?}");
    drop(meta);

    let mut failed = 0_u32;
    for fail_after in 0..192 {
        let meta = MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        fault.arm(fail_after);
        let repaired = repair_summary_rows(&meta, &hosted());
        fault.disable();
        drop(meta);

        let meta = MetaStore::reopen_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
        let settled = audit_summary_rows(&meta, &hosted()).unwrap();
        assert!(
            settled == damaged || settled.is_empty(),
            "injecting after {fail_after} reads left a half-repaired store: {settled:?}"
        );
        failed += u32::from(repaired.is_err());
    }

    assert!(failed > 0, "no injection point reached the repair");
}
