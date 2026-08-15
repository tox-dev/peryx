use super::store;

#[test]
fn test_analytics_snapshot_is_absent_before_first_save() {
    let (_dir, meta) = store();
    assert_eq!(meta.analytics().load().unwrap(), None);
}

#[test]
fn test_analytics_save_then_load_round_trips_the_snapshot() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    handle.save(b"first").unwrap();
    assert_eq!(handle.load().unwrap(), Some(b"first".to_vec()));
    handle.save(b"second").unwrap();
    assert_eq!(handle.load().unwrap(), Some(b"second".to_vec()));
}

#[test]
fn test_analytics_handle_shares_the_store_database() {
    let (_dir, meta) = store();
    meta.analytics().save(b"snapshot").unwrap();
    assert_eq!(meta.analytics().load().unwrap(), Some(b"snapshot".to_vec()));
}

#[test]
fn test_analytics_handle_is_a_noop_once_the_store_drops() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    drop(meta);
    handle.save(b"ignored").unwrap();
    assert_eq!(handle.load().unwrap(), None);
}

#[test]
fn test_analytics_daily_snapshot_round_trips_under_its_own_key() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    assert_eq!(handle.load_daily().unwrap(), None);
    handle.save_daily(b"daily").unwrap();
    assert_eq!(handle.load_daily().unwrap(), Some(b"daily".to_vec()));
    assert_eq!(handle.load().unwrap(), None);
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
    assert_eq!(handle.load().unwrap(), None);
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
