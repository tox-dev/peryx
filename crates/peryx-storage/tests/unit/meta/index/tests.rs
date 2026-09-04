use super::{MetaError, MetaScanError, MetaStore};

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
fn test_driver_read_txn_keeps_dependent_reads_in_one_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    assert!(meta.read_driver_txn(|txn| txn.prefix("catalog/")).unwrap().is_empty());
    assert!(meta.read_driver_txn(|txn| txn.prefixes(&[])).unwrap().is_empty());
    for (key, value) in [
        ("catalog/2", b"two".as_slice()),
        ("other/1", b"other".as_slice()),
        ("catalog/1", b"one".as_slice()),
    ] {
        meta.put_driver_value(key, value).unwrap();
    }

    assert_eq!(
        meta.read_driver_txn(|txn| {
            assert_eq!(txn.get("catalog/1")?, Some(b"one".to_vec()));
            meta.put_driver_value("catalog/3", b"three")?;
            txn.prefixes(&["catalog/", "other/"])
        })
        .unwrap(),
        vec![
            vec![
                ("catalog/1".to_owned(), b"one".to_vec()),
                ("catalog/2".to_owned(), b"two".to_vec()),
            ],
            vec![("other/1".to_owned(), b"other".to_vec())],
        ]
    );
    drop(meta);
    let read_only = MetaStore::open_existing_read_only(path).unwrap();
    assert_eq!(
        read_only.read_driver_txn(|txn| txn.prefix("catalog/")).unwrap().len(),
        3
    );
}

#[test]
fn test_driver_read_txn_prefix_keys_limited_bounds_one_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    for key in ["catalog/1", "catalog/2", "other/1"] {
        meta.put_driver_value(key, b"value").unwrap();
    }

    assert_eq!(
        meta.read_driver_txn(|txn| {
            let bounded = txn.prefix_keys_limited("catalog/", 1).unwrap();
            meta.put_driver_value("catalog/3", b"value").unwrap();
            Ok::<_, MetaError>((
                txn.prefix_keys_limited("catalog/", 0).unwrap(),
                bounded,
                txn.prefix_keys_limited("catalog/", 9).unwrap(),
            ))
        })
        .unwrap(),
        (
            Vec::new(),
            vec!["catalog/1".to_owned()],
            vec!["catalog/1".to_owned(), "catalog/2".to_owned()],
        )
    );
    assert_eq!(
        meta.read_driver_txn(|txn| txn.prefix_keys_limited("catalog/", 9))
            .unwrap(),
        vec!["catalog/1", "catalog/2", "catalog/3"]
    );
}

#[test]
fn test_driver_read_txn_reports_snapshot_read_failures() {
    let (meta, _backend, fault) = super::super::fault::initialized();
    fault.arm(0);
    assert!(meta.read_driver_txn(|txn| txn.get("catalog/1")).is_err());
    fault.disable();
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
    let mut snapshot = None;

    meta.visit_driver_policy_snapshot(
        "catalog/",
        "private",
        |generation| {
            snapshot = Some(generation);
            Ok::<(), std::io::Error>(())
        },
        |key, value| {
            visited.push((key.to_owned(), value.to_vec()));
            if visited.len() == 1 {
                meta.put_driver_value("catalog/3", b"three").unwrap();
                meta.advance_policy_generation("private").unwrap();
            }
            Ok::<(), std::io::Error>(())
        },
    )
    .unwrap();

    assert_eq!(
        visited,
        vec![
            ("catalog/1".to_owned(), b"one".to_vec()),
            ("catalog/2".to_owned(), b"two".to_vec())
        ]
    );
    assert_eq!(snapshot.unwrap().policy, 1);
    assert_eq!(meta.policy_input_generation("private").unwrap().policy, 2);
    let error = meta
        .visit_driver_policy_snapshot(
            "catalog/",
            "private",
            |_| Ok(()),
            |_key, _value| Err(std::io::Error::other("stop")),
        )
        .unwrap_err();
    assert!(matches!(error, MetaScanError::Visit(_)));
}

#[test]
fn test_visit_driver_policy_snapshot_ignores_another_repository() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let mut generations = Vec::new();
    let mut snapshot = |repository: &str| {
        meta.visit_driver_policy_snapshot(
            "catalog/",
            repository,
            |generation| {
                generations.push(generation);
                Ok::<(), std::io::Error>(())
            },
            |_key, _value| Ok(()),
        )
        .unwrap();
    };

    snapshot("private");
    meta.commit_driver_txn(|txn| {
        txn.touch_policy_inputs("other");
        txn.put("catalog/1", b"one")?;
        Ok::<_, MetaError>(((), vec![b"entry".to_vec()]))
    })
    .unwrap();
    snapshot("private");
    meta.commit_driver_txn(|txn| {
        txn.touch_policy_inputs("private");
        txn.put("catalog/2", b"two")?;
        Ok::<_, MetaError>(((), vec![b"entry".to_vec()]))
    })
    .unwrap();
    snapshot("private");

    assert_eq!(
        generations
            .into_iter()
            .map(|generation| generation.repository)
            .collect::<Vec<_>>(),
        vec![0, 0, 1]
    );
}

#[test]
fn test_remove_driver_values_if_honors_zero_limit() {
    fn remove(value: &[u8]) -> Result<bool, MetaError> {
        serde_json::from_slice(value).map_err(MetaError::from)
    }

    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.put_driver_value("catalog/1", b"false").unwrap();
    meta.put_driver_value("other/1", b"other").unwrap();

    assert!(meta.remove_driver_values_if("catalog/", 0, remove).unwrap().is_empty());
    assert!(meta.remove_driver_values_if("catalog/", 10, remove).unwrap().is_empty());
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

/// A bounded prefix scan that fails must not come back as a shorter list. A short list reads as
/// fewer matching keys, and a caller reading back a generation it already chose from this snapshot
/// would take the short list for the whole of it.
///
/// A store handle does not survive its own injected failure, so each step reopens the retained pages
/// rather than reusing one handle.
#[test]
fn driver_prefix_keys_limited_never_returns_a_short_list() {
    let (meta, inner, fault) = crate::meta::fault::initialized();
    for key in ["catalog/1", "catalog/2", "catalog/3", "other/1"] {
        meta.put_driver_value(key, b"value").unwrap();
    }
    let whole = meta
        .read_driver_txn(|txn| txn.prefix_keys_limited("catalog/", 10))
        .unwrap();
    assert_eq!(whole.len(), 3);
    drop(meta);

    let mut failed = 0_u32;
    for fail_after in 0..192 {
        let meta = crate::meta::fault::reopen(&inner, &fault);
        fault.arm(fail_after);
        let listed = meta.read_driver_txn(|txn| txn.prefix_keys_limited("catalog/", 10));
        fault.disable();
        match listed {
            Ok(keys) => assert_eq!(keys, whole, "injecting after {fail_after} reads listed short"),
            Err(_) => failed += 1,
        }
    }

    assert!(failed > 0, "no injection point reached the scan");
}

/// The same guarantee for the unbounded collect: a scan that fails partway must not present the rows
/// it had gathered as the whole prefix.
#[test]
fn driver_prefix_never_returns_a_short_collection() {
    let (meta, inner, fault) = crate::meta::fault::initialized();
    for key in ["catalog/1", "catalog/2", "catalog/3", "other/1"] {
        meta.put_driver_value(key, b"value").unwrap();
    }
    let whole = meta.read_driver_txn(|txn| txn.prefix("catalog/")).unwrap();
    assert_eq!(whole.len(), 3);
    drop(meta);

    let mut failed = 0_u32;
    for fail_after in 0..192 {
        let meta = crate::meta::fault::reopen(&inner, &fault);
        fault.arm(fail_after);
        let collected = meta.read_driver_txn(|txn| txn.prefix("catalog/"));
        fault.disable();
        match collected {
            Ok(entries) => assert_eq!(entries, whole, "injecting after {fail_after} reads collected short"),
            Err(_) => failed += 1,
        }
    }

    assert!(failed > 0, "no injection point reached the scan");
}
