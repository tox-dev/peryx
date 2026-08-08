use std::collections::BTreeMap;

use super::MetaStore;

#[test]
fn test_view_frontier_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    assert_eq!(meta.view_frontier("search").unwrap(), None);
    assert_eq!(meta.view_frontiers().unwrap(), BTreeMap::new());
}

#[test]
fn test_set_view_frontier_advances_and_reports_the_stored_serial() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    assert_eq!(meta.set_view_frontier("search", 3).unwrap(), 3);
    assert_eq!(meta.view_frontier("search").unwrap(), Some(3));
    assert_eq!(meta.set_view_frontier("search", 7).unwrap(), 7);
    assert_eq!(meta.view_frontier("search").unwrap(), Some(7));
}

#[test]
fn test_set_view_frontier_never_moves_backward() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    assert_eq!(meta.set_view_frontier("search", 5).unwrap(), 5);
    assert_eq!(meta.set_view_frontier("search", 2).unwrap(), 5);
    assert_eq!(meta.view_frontier("search").unwrap(), Some(5));
}

#[test]
fn test_view_frontiers_lists_every_view_in_name_order() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.set_view_frontier("search", 4).unwrap();
    meta.set_view_frontier("cache", 2).unwrap();

    assert_eq!(
        meta.view_frontiers().unwrap(),
        BTreeMap::from([("cache".to_owned(), 2), ("search".to_owned(), 4)])
    );
}

#[test]
fn test_view_frontier_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    {
        let meta = MetaStore::open(&path).unwrap();
        meta.set_view_frontier("search", 9).unwrap();
    }
    let meta = MetaStore::open_existing(&path).unwrap();
    assert_eq!(meta.view_frontier("search").unwrap(), Some(9));
}
