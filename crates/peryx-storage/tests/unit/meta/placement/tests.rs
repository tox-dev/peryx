use rstest::rstest;

use super::{
    ArtifactOrigin, ArtifactPlacement, ArtifactPlacementPage, ArtifactPlacementQuery, ArtifactPlacementQueryError,
    ArtifactPlacementRow, ArtifactSource, ByteAvailability, MetaStore, PlacementEvent,
};

struct Cached;
impl ArtifactOrigin for Cached {
    fn artifact_source(&self) -> ArtifactSource {
        ArtifactSource::Proxy
    }
}

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

#[test]
fn test_source_and_availability_wire_spellings_round_trip() {
    for (source, wire) in [
        (ArtifactSource::Hosted, "hosted"),
        (ArtifactSource::Proxy, "proxy"),
        (ArtifactSource::Generated, "generated"),
    ] {
        assert_eq!(source.as_str(), wire);
        assert_eq!(serde_json::to_string(&source).unwrap(), format!("\"{wire}\""));
    }
    for (availability, wire) in [
        (ByteAvailability::Local, "local"),
        (ByteAvailability::RemoteOnly, "remote_only"),
        (ByteAvailability::Unavailable, "unavailable"),
    ] {
        assert_eq!(availability.as_str(), wire);
        assert_eq!(serde_json::to_string(&availability).unwrap(), format!("\"{wire}\""));
    }
    assert!(ByteAvailability::Local.is_local());
    assert!(!ByteAvailability::RemoteOnly.is_local());
}

#[test]
fn test_the_four_fixtures_project_distinct_availability() {
    assert_eq!(
        ArtifactPlacement::record(ArtifactSource::Hosted, true).availability,
        ByteAvailability::Local
    );
    assert_eq!(
        ArtifactPlacement::record(ArtifactSource::Proxy, true).availability,
        ByteAvailability::Local
    );
    assert_eq!(
        ArtifactPlacement::record(ArtifactSource::Proxy, false).availability,
        ByteAvailability::RemoteOnly
    );
    assert_eq!(
        ArtifactPlacement::record(ArtifactSource::Hosted, false).availability,
        ByteAvailability::Unavailable
    );
    assert_eq!(
        ArtifactPlacement::record(ArtifactSource::Generated, false).availability,
        ByteAvailability::Unavailable
    );
    assert_eq!(Cached.artifact_source(), ArtifactSource::Proxy);
    assert!(ArtifactSource::Proxy.has_upstream());
    assert!(!ArtifactSource::Generated.has_upstream());
}

#[test]
fn test_a_failed_write_keeps_prior_state_and_never_fabricates_local() {
    let verified = ArtifactPlacement::record(ArtifactSource::Proxy, true);
    assert_eq!(verified.after(PlacementEvent::WriteFailed), verified);

    let empty = ArtifactPlacement::record(ArtifactSource::Proxy, false);
    assert_eq!(
        empty.after(PlacementEvent::WriteFailed).availability,
        ByteAvailability::RemoteOnly
    );

    let hosted = ArtifactPlacement::record(ArtifactSource::Hosted, false);
    assert_eq!(
        hosted.after(PlacementEvent::WriteFailed).availability,
        ByteAvailability::Unavailable
    );
}

#[test]
fn test_events_move_availability_but_keep_the_source() {
    let proxy = ArtifactPlacement::record(ArtifactSource::Proxy, false);
    let filled = proxy.after(PlacementEvent::BytesVerified);
    assert_eq!(filled.availability, ByteAvailability::Local);
    assert_eq!(filled.source, ArtifactSource::Proxy);
    assert_eq!(
        filled.after(PlacementEvent::BytesRemoved).availability,
        ByteAvailability::RemoteOnly
    );
    assert_eq!(
        filled.after(PlacementEvent::Repaired { present: false }).availability,
        ByteAvailability::RemoteOnly
    );
    assert_eq!(
        proxy.after(PlacementEvent::Repaired { present: true }).availability,
        ByteAvailability::Local
    );
}

#[test]
fn test_placement_round_trips_through_the_store() {
    let (_dir, meta) = store();
    assert_eq!(meta.get_artifact_placement("aa").unwrap(), None);
    let stored = meta
        .record_artifact_placement("aa", ArtifactSource::Proxy, false)
        .unwrap();
    assert_eq!(stored.availability, ByteAvailability::RemoteOnly);
    assert_eq!(meta.get_artifact_placement("aa").unwrap(), Some(stored));
    assert!(meta.delete_artifact_placement("aa").unwrap());
    assert!(!meta.delete_artifact_placement("aa").unwrap());
    assert_eq!(meta.get_artifact_placement("aa").unwrap(), None);
}

#[test]
fn test_ensure_records_once_and_keeps_a_cached_state() {
    let (_dir, meta) = store();
    let created = meta.ensure_artifact_placement("aa", ArtifactSource::Proxy).unwrap();
    assert_eq!(created.availability, ByteAvailability::RemoteOnly);
    meta.apply_placement_event("aa", PlacementEvent::BytesVerified).unwrap();
    // A re-discovery must not reset the now-cached artifact back to remote-only.
    let reobserved = meta.ensure_artifact_placement("aa", ArtifactSource::Proxy).unwrap();
    assert_eq!(reobserved.availability, ByteAvailability::Local);
}

#[test]
fn test_apply_event_updates_or_reports_absence() {
    let (_dir, meta) = store();
    assert_eq!(
        meta.apply_placement_event("missing", PlacementEvent::BytesVerified)
            .unwrap(),
        None
    );
    meta.record_artifact_placement("aa", ArtifactSource::Proxy, false)
        .unwrap();
    let filled = meta
        .apply_placement_event("aa", PlacementEvent::BytesVerified)
        .unwrap()
        .unwrap();
    assert_eq!(filled.availability, ByteAvailability::Local);
    // A no-op event leaves the row untouched but still returns the current value.
    let unchanged = meta
        .apply_placement_event("aa", PlacementEvent::Repaired { present: true })
        .unwrap()
        .unwrap();
    assert_eq!(unchanged, filled);
    assert_eq!(meta.get_artifact_placement("aa").unwrap(), Some(filled));
}

#[test]
fn test_repair_reconciles_a_stale_projection_in_bounded_batches() {
    let (_dir, meta) = store();
    // Record two proxied artifacts as remote-only, then let their bytes appear locally.
    for digest in ["a1", "a2", "a3"] {
        meta.record_artifact_placement(digest, ArtifactSource::Proxy, false)
            .unwrap();
    }
    let local = ["a1", "a3"];

    let first = meta
        .repair_artifact_placements(None, 2, |digest| local.contains(&digest))
        .unwrap();
    assert_eq!(first.scanned, 2);
    assert_eq!(first.reconciled, 1);
    assert_eq!(first.next_cursor.as_deref(), Some("a2"));
    assert_eq!(
        meta.get_artifact_placement("a1").unwrap().unwrap().availability,
        ByteAvailability::Local
    );
    assert_eq!(
        meta.get_artifact_placement("a2").unwrap().unwrap().availability,
        ByteAvailability::RemoteOnly
    );

    let second = meta
        .repair_artifact_placements(first.next_cursor.as_deref(), 2, |digest| local.contains(&digest))
        .unwrap();
    assert_eq!(second.scanned, 1);
    assert_eq!(second.reconciled, 1);
    assert_eq!(second.next_cursor, None);
    assert_eq!(
        meta.get_artifact_placement("a3").unwrap().unwrap().availability,
        ByteAvailability::Local
    );
}

fn row(digest: &str, source: ArtifactSource, availability: ByteAvailability) -> ArtifactPlacementRow {
    ArtifactPlacementRow {
        digest: digest.to_owned(),
        source,
        availability,
    }
}

#[test]
fn test_list_on_an_empty_table_returns_no_rows() {
    let (_dir, meta) = store();
    let page = meta
        .list_artifact_placements(&ArtifactPlacementQuery::default())
        .unwrap();
    assert!(page.rows.is_empty());
    assert_eq!(page.next_cursor, None);
}

#[test]
fn test_list_returns_rows_in_digest_order_with_source_and_availability() {
    let (_dir, meta) = store();
    meta.record_artifact_placement("a2", ArtifactSource::Hosted, true)
        .unwrap();
    meta.record_artifact_placement("a1", ArtifactSource::Proxy, false)
        .unwrap();
    let page = meta
        .list_artifact_placements(&ArtifactPlacementQuery::default())
        .unwrap();
    assert_eq!(
        page.rows,
        vec![
            row("a1", ArtifactSource::Proxy, ByteAvailability::RemoteOnly),
            row("a2", ArtifactSource::Hosted, ByteAvailability::Local),
        ]
    );
    assert_eq!(page.next_cursor, None);
}

#[test]
fn test_list_paginates_after_an_exclusive_cursor() {
    let (_dir, meta) = store();
    for digest in ["a1", "a2", "a3"] {
        meta.record_artifact_placement(digest, ArtifactSource::Proxy, false)
            .unwrap();
    }
    let first = meta
        .list_artifact_placements(&ArtifactPlacementQuery { cursor: None, limit: 2 })
        .unwrap();
    assert_eq!(
        first.rows.iter().map(|r| r.digest.as_str()).collect::<Vec<_>>(),
        ["a1", "a2"]
    );
    assert_eq!(first.next_cursor.as_deref(), Some("a2"));
    let second = meta
        .list_artifact_placements(&ArtifactPlacementQuery {
            cursor: first.next_cursor,
            limit: 2,
        })
        .unwrap();
    assert_eq!(
        second.rows.iter().map(|r| r.digest.as_str()).collect::<Vec<_>>(),
        ["a3"]
    );
    assert_eq!(second.next_cursor, None);
}

#[test]
fn test_list_page_that_exactly_fills_carries_no_next_cursor() {
    let (_dir, meta) = store();
    meta.record_artifact_placement("a1", ArtifactSource::Proxy, false)
        .unwrap();
    meta.record_artifact_placement("a2", ArtifactSource::Proxy, false)
        .unwrap();
    let page = meta
        .list_artifact_placements(&ArtifactPlacementQuery { cursor: None, limit: 2 })
        .unwrap();
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.next_cursor, None);
}

#[test]
fn test_a_placement_page_serializes_to_the_view_wire_shape() {
    let page = ArtifactPlacementPage {
        rows: vec![row("aa", ArtifactSource::Proxy, ByteAvailability::RemoteOnly)],
        next_cursor: Some("aa".to_owned()),
    };
    assert_eq!(
        serde_json::to_value(&page).unwrap(),
        serde_json::json!({
            "rows": [{"digest": "aa", "source": "proxy", "availability": "remote_only"}],
            "next_cursor": "aa",
        })
    );
}

#[rstest]
#[case(0)]
#[case(101)]
fn test_list_rejects_an_out_of_range_limit(#[case] limit: usize) {
    let (_dir, meta) = store();
    let error = meta
        .list_artifact_placements(&ArtifactPlacementQuery { cursor: None, limit })
        .unwrap_err();
    assert!(matches!(error, ArtifactPlacementQueryError::InvalidLimit));
}

#[test]
fn test_repair_with_a_zero_limit_reads_nothing() {
    let (_dir, meta) = store();
    meta.record_artifact_placement("a1", ArtifactSource::Proxy, false)
        .unwrap();
    let page = meta.repair_artifact_placements(None, 0, |_| true).unwrap();
    assert_eq!(page.scanned, 0);
    assert_eq!(page.reconciled, 0);
    assert_eq!(page.next_cursor, None);
}
