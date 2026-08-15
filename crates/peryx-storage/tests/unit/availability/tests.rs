use peryx_core::AnalyticsSnapshotStore as _;

#[test]
fn metadata_store_exposes_an_empty_analytics_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::meta::MetaStore::open(directory.path().join("meta.redb")).unwrap();

    assert_eq!(store.load_analytics_snapshot().unwrap(), None);
}

#[test]
fn metadata_store_maps_an_analytics_table_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("meta.redb");
    let database = redb::Database::create(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, u64>::new("analytics"))
        .unwrap();
    transaction.commit().unwrap();
    drop(database);
    let store = crate::meta::MetaStore::open_existing(path).unwrap();
    let expected = store.analytics().load_apply().unwrap_err().to_string();

    assert_eq!(store.load_analytics_snapshot().unwrap_err().to_string(), expected);
}
