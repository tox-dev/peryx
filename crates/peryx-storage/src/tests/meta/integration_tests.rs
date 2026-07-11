use crate::meta::MetaStore;

#[test]
fn test_open_existing_requires_database_file() {
    let dir = tempfile::tempdir().unwrap();
    assert!(MetaStore::open_existing(dir.path().join("missing.redb")).is_err());
}

#[test]
fn test_journaled_batch_applies_deletes_and_advances_the_serial() {
    use crate::meta::DriverBatch;

    let (_dir, store) = super::store();
    store
        .put_driver_value("pypi\u{0}u\u{0}hosted/flask/flask-1.0.whl", b"row")
        .unwrap();
    let mut batch = DriverBatch::new();
    batch.delete("pypi\u{0}u\u{0}hosted/flask/flask-1.0.whl".to_owned());

    let serial = store
        .commit_driver_batch_journaled(&batch, b"{\"action\":\"delete\"}")
        .unwrap();

    assert_eq!(serial, 1, "the journaled commit allocates the next serial");
    assert!(
        store
            .get_driver_value("pypi\u{0}u\u{0}hosted/flask/flask-1.0.whl")
            .unwrap()
            .is_none(),
        "the delete in the batch removed the row"
    );
}
