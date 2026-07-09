use velodex_storage::blob::BlobStore;
use velodex_storage::meta::MetaStore;

use crate::operator;

use super::valid_backup;

#[test]
fn test_backup_restore_roundtrip_restores_metadata_and_blobs() {
    let (_source, root, _config, backup, content_digest, metadata_digest) = valid_backup();
    let restored = root.path().join("restored");
    let mut out = Vec::new();

    std::fs::create_dir(&restored).unwrap();
    operator::restore(&backup, &restored, false, &mut out).unwrap();

    let meta = MetaStore::open_existing(restored.join("velodex.redb")).unwrap();
    assert_eq!(meta.list_projects("hosted").unwrap(), vec!["Flask"]);
    assert_eq!(meta.list_upload_entries("hosted", "flask").unwrap().len(), 1);
    let blobs = BlobStore::new(restored.join("blobs"));
    assert_eq!(blobs.read(&content_digest).unwrap(), b"wheel bytes");
    assert_eq!(blobs.read(&metadata_digest).unwrap(), b"metadata bytes");
}
