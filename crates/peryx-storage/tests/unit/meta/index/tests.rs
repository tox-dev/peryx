use super::{MetaScanError, MetaStore};

#[test]
fn test_driver_prefix_keys_limited_bounds_results() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    for key in ["catalog/1", "catalog/2", "catalog/3", "other/1"] {
        meta.put_driver_value(key, b"value").unwrap();
    }

    assert!(meta.driver_prefix_keys_limited("catalog/", 0).unwrap().is_empty());
    assert_eq!(
        meta.driver_prefix_keys_limited("catalog/", 2).unwrap(),
        vec!["catalog/1", "catalog/2"]
    );
    assert_eq!(meta.driver_prefix_keys("other/").unwrap(), vec!["other/1"]);
    let mut visited = Vec::new();
    meta.visit_driver_prefix("catalog/", |key, value| {
        visited.push((key.to_owned(), value.to_vec()));
    })
    .unwrap();
    assert_eq!(visited.len(), 3);
}

#[test]
fn test_visit_driver_policy_snapshot_is_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.put_driver_value("catalog/1", b"one").unwrap();
    meta.put_driver_value("catalog/2", b"two").unwrap();
    meta.put_driver_value("other/1", b"other").unwrap();
    meta.advance_policy_generation("private").unwrap();
    let mut visited = Vec::new();

    let snapshot = meta
        .visit_driver_policy_snapshot("catalog/", "private", |key, value| {
            visited.push((key.to_owned(), value.to_vec()));
            if visited.len() == 1 {
                meta.put_driver_value("catalog/3", b"three").unwrap();
                meta.advance_policy_generation("private").unwrap();
            }
            Ok::<(), std::io::Error>(())
        })
        .unwrap();

    assert_eq!(
        visited,
        vec![
            ("catalog/1".to_owned(), b"one".to_vec()),
            ("catalog/2".to_owned(), b"two".to_vec())
        ]
    );
    assert_eq!(snapshot.policy, 1);
    assert_eq!(meta.policy_input_generation("private").unwrap().policy, 2);
    let error = meta
        .visit_driver_policy_snapshot("catalog/", "private", |_key, _value| Err(std::io::Error::other("stop")))
        .unwrap_err();
    assert!(matches!(error, MetaScanError::Visit(_)));
}

#[test]
fn test_remove_driver_values_if_honors_zero_limit() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    assert!(
        meta.remove_driver_values_if("catalog/", 0, |_| Ok(true))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn test_remove_driver_values_if_stops_after_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.put_driver_value("catalog/1", b"one").unwrap();
    meta.put_driver_value("other/1", b"other").unwrap();

    assert!(
        meta.remove_driver_values_if("catalog/", 10, |_| Ok(false))
            .unwrap()
            .is_empty()
    );
}
