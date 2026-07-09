use std::collections::HashMap;

use super::store;
use crate::meta::{FileSource, MetaStore};

#[test]
fn test_put_and_get_file_url() {
    let (_dir, store) = store();
    assert_eq!(store.get_file_url("deadbeef").unwrap(), None);
    store
        .put_file_url("deadbeef", "https://files.example/pkg.whl", "pypi")
        .unwrap();
    assert_eq!(
        store.get_file_url("deadbeef").unwrap(),
        Some(FileSource {
            url: "https://files.example/pkg.whl".to_owned(),
            source: "pypi".to_owned(),
            size: None,
        })
    );
}

#[test]
fn test_put_and_get_metadata() {
    let (_dir, store) = store();
    assert_eq!(store.get_metadata("wheelsha").unwrap(), None);
    store
        .put_metadata("wheelsha", "https://up/pkg.whl.metadata", "metasha", "pypi")
        .unwrap();
    assert_eq!(
        store.get_metadata("wheelsha").unwrap(),
        Some((
            "https://up/pkg.whl.metadata".to_owned(),
            "metasha".to_owned(),
            "pypi".to_owned()
        ))
    );
}

#[test]
fn test_get_metadata_digests_skips_missing_and_malformed_records() {
    let (dir, store) = store();
    store
        .put_metadata("wheelsha", "https://up/pkg.whl.metadata", "metasha", "pypi")
        .unwrap();
    drop(store);
    {
        let db = redb::Database::create(dir.path().join("velodex.redb")).unwrap();
        let table: redb::TableDefinition<&str, &str> = redb::TableDefinition::new("metadata_sidecar");
        let txn = db.begin_write().unwrap();
        txn.open_table(table).unwrap().insert("broken", "only-url").unwrap();
        txn.commit().unwrap();
    }
    let store = MetaStore::open(dir.path().join("velodex.redb")).unwrap();

    let digests = store.get_metadata_digests(["missing", "broken", "wheelsha"]).unwrap();

    assert_eq!(digests, HashMap::from([("wheelsha".to_owned(), "metasha".to_owned())]));
}
