use super::store;
use crate::meta::AnalyticsCheckpoint;

#[test]
fn test_analytics_snapshot_is_absent_before_first_save() {
    let (_dir, meta) = store();
    assert_eq!(
        meta.analytics().load_checkpoint().unwrap(),
        AnalyticsCheckpoint::default()
    );
}

#[test]
fn test_analytics_save_then_load_round_trips_the_checkpoint() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    handle.save_checkpoint(b"first lifetime", b"first daily").unwrap();
    assert_eq!(
        handle.load_checkpoint().unwrap(),
        AnalyticsCheckpoint {
            lifetime: Some(b"first lifetime".to_vec()),
            daily: Some(b"first daily".to_vec()),
        }
    );
    handle.save_checkpoint(b"second lifetime", b"second daily").unwrap();
    assert_eq!(
        handle.load_checkpoint().unwrap(),
        AnalyticsCheckpoint {
            lifetime: Some(b"second lifetime".to_vec()),
            daily: Some(b"second daily".to_vec()),
        }
    );
}

#[test]
fn test_analytics_handle_shares_the_store_database() {
    let (_dir, meta) = store();
    meta.analytics().save_checkpoint(b"lifetime", b"daily").unwrap();
    assert_eq!(
        meta.analytics().load_checkpoint().unwrap(),
        AnalyticsCheckpoint {
            lifetime: Some(b"lifetime".to_vec()),
            daily: Some(b"daily".to_vec()),
        }
    );
}

#[test]
fn test_analytics_handle_is_a_noop_once_the_store_drops() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    handle.save_apply(b"retained apply").unwrap();
    handle.save_producer(b"retained producer").unwrap();
    assert_eq!(
        (handle.load_apply().unwrap(), handle.load_producer().unwrap()),
        (Some(b"retained apply".to_vec()), Some(b"retained producer".to_vec()))
    );
    drop(meta);
    handle.save_checkpoint(b"ignored", b"ignored").unwrap();
    handle.save_apply(b"ignored").unwrap();
    handle.save_producer(b"ignored").unwrap();
    assert_eq!(handle.load_checkpoint().unwrap(), AnalyticsCheckpoint::default());
    assert_eq!(
        (handle.load_apply().unwrap(), handle.load_producer().unwrap()),
        (None, None)
    );
}

#[test]
fn test_analytics_apply_state_is_absent_before_first_save() {
    let (_dir, meta) = store();
    assert_eq!(meta.analytics().load_apply().unwrap(), None);
}

#[test]
fn test_analytics_apply_state_round_trips_under_its_own_key() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    handle.save_apply(b"apply").unwrap();
    assert_eq!(handle.load_apply().unwrap(), Some(b"apply".to_vec()));
    assert_eq!(handle.load_checkpoint().unwrap(), AnalyticsCheckpoint::default());
    assert_eq!(handle.load_producer().unwrap(), None);
}

#[test]
fn test_analytics_producer_record_is_absent_before_first_save() {
    let (_dir, meta) = store();
    assert_eq!(meta.analytics().load_producer().unwrap(), None);
}

#[test]
fn test_analytics_producer_record_round_trips_under_its_own_key() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    handle.save_producer(b"producer").unwrap();
    assert_eq!(handle.load_producer().unwrap(), Some(b"producer".to_vec()));
    assert_eq!(handle.load_apply().unwrap(), None);
}
