#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use peryx_storage::meta::MetaStore;
use rstest::rstest;

use crate::operator;

use super::{FailOnLine, blob_relpath, claimed_data_dir, identified_backup, valid_backup};

#[rstest]
#[case::non_empty_directory(create_non_empty_directory, "not empty")]
#[case::file(create_file, "exists and is not a directory")]
fn test_restore_retries_occupied_target_with_force(#[case] create: fn(&Path), #[case] expected: &str) {
    let (_source, root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    let restored = root.path().join("restored");
    create(&restored);

    let err = operator::restore(&backup, &restored, false, &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains(expected));

    operator::restore(&backup, &restored, true, &mut Vec::new()).unwrap();
    assert_eq!(
        (
            restored.is_dir(),
            restored.join("peryx.redb").is_file(),
            restored.join("blocker").exists()
        ),
        (true, true, false)
    );
}

#[test]
fn test_restore_refuses_backup_with_verification_errors() {
    let (_source, root, _config, backup, content_digest, _metadata_digest) = valid_backup();
    std::fs::remove_file(backup.join(blob_relpath(&content_digest))).unwrap();

    let err = operator::restore(&backup, &root.path().join("restored"), false, &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(err.to_string().contains("problem\tblob"));
}

#[test]
fn test_restore_warns_when_config_data_dir_changes() {
    let (_source, root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    let restored = root.path().join("restored");

    let mut out = Vec::new();
    operator::restore(&backup, &restored, false, &mut out).unwrap();

    assert!(String::from_utf8(out).unwrap().contains("warning\tconfig\tdata_dir"));
}

#[test]
fn test_restore_skips_config_warning_when_data_dir_matches() {
    let (_source, _root, config, backup, _content_digest, _metadata_digest) = valid_backup();

    let mut out = Vec::new();
    let err = operator::restore(&backup, &config.data_dir, false, &mut out).unwrap_err();

    assert!(err.to_string().contains("not empty"));
    assert!(!String::from_utf8(out).unwrap().contains("warning\tconfig\tdata_dir"));
}

#[test]
fn test_restore_force_publishes_the_full_backup_and_leaves_no_staging_siblings() {
    let (_source, root, _config, backup, content_digest, _metadata_digest) = valid_backup();
    let restored = root.path().join("restored");
    create_non_empty_directory(&restored);

    operator::restore(&backup, &restored, true, &mut Vec::new()).unwrap();

    assert!(restored.join("peryx.redb").is_file());
    assert!(restored.join(blob_relpath(&content_digest)).is_file());
    assert!(!restored.join("blocker").exists());
    assert!(!root.path().join("restored.restore-staging").exists());
    assert!(!root.path().join("restored.restore-old").exists());
}

#[test]
fn test_restore_refuses_a_backup_that_aliases_the_target() {
    let (_source, _root, _config, backup, _content_digest, _metadata_digest) = valid_backup();

    let err = operator::restore(&backup, &backup, true, &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("onto itself"), "{err}");
}

#[cfg(unix)]
#[test]
fn test_restore_keeps_the_prior_target_intact_when_staging_cannot_be_created() {
    let (_source, root, _config, backup, _content_digest, _metadata_digest) = valid_backup();
    let restored = root.path().join("restored");
    std::fs::create_dir(&restored).unwrap();
    std::fs::write(restored.join("keep"), b"old").unwrap();
    let previous = std::fs::metadata(root.path()).unwrap().permissions().mode();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

    let err = operator::restore(&backup, &restored, true, &mut Vec::new()).unwrap_err();

    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(previous)).unwrap();
    assert!(err.to_string().contains("staging directory"), "{err}");
    assert_eq!(std::fs::read(restored.join("keep")).unwrap(), b"old");
    assert!(!root.path().join("restored.restore-staging").exists());
    assert!(!root.path().join("restored.restore-old").exists());
}

fn create_non_empty_directory(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::write(path.join("blocker"), b"x").unwrap();
}

fn create_file(path: &Path) {
    std::fs::write(path, b"x").unwrap();
}

#[test]
fn test_restore_refuses_a_directory_claimed_by_a_different_node() {
    let (_holder, backup) = identified_backup("node-a", 1);
    let target = claimed_data_dir("node-b", 1);

    let err = operator::restore(&backup, target.path(), true, &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("refusing to restore node node-a"), "{err}");
    assert!(err.to_string().contains("claimed by node node-b"), "{err}");
    // The target keeps its own store: a split-brain restore never touched it.
    let meta = MetaStore::open_existing_read_only(target.path().join("peryx.redb")).unwrap();
    assert_eq!(meta.writer_identity().unwrap().as_deref(), Some("node-b"));
}

#[test]
fn test_restore_warns_on_a_rollback_of_the_same_node() {
    let (_holder, backup) = identified_backup("node-a", 1);
    let target = claimed_data_dir("node-a", 3);
    let mut out = Vec::new();

    operator::restore(&backup, target.path(), true, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("warning\trestore\trollback"), "{text}");
    assert!(text.contains("restored"), "{text}");
}

#[test]
fn test_restore_propagates_a_rollback_warning_write_error() {
    let (_holder, backup) = identified_backup("node-a", 1);
    let target = claimed_data_dir("node-a", 3);
    let mut out = FailOnLine {
        needle: "rollback",
        ..FailOnLine::default()
    };

    operator::restore(&backup, target.path(), true, &mut out).unwrap_err();

    assert!(out.seen.contains("warning\trestore\trollback"), "{}", out.seen);
    // The write failed inside the preflight, before the target was touched.
    let meta = MetaStore::open_existing_read_only(target.path().join("peryx.redb")).unwrap();
    assert_eq!(meta.writer_identity().unwrap().as_deref(), Some("node-a"));
}

#[test]
fn test_restore_replaces_a_same_node_target_without_a_rollback_warning() {
    let (_holder, backup) = identified_backup("node-a", 2);
    let target = claimed_data_dir("node-a", 0);
    let mut out = Vec::new();

    operator::restore(&backup, target.path(), true, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains("rollback"), "{text}");
    let meta = MetaStore::open_existing_read_only(target.path().join("peryx.redb")).unwrap();
    assert_eq!(meta.writer_identity().unwrap().as_deref(), Some("node-a"));
}

#[test]
fn test_restore_ignores_an_unreadable_target_store() {
    let (_holder, backup) = identified_backup("node-a", 1);
    let target = tempfile::tempdir().unwrap();
    std::fs::write(target.path().join("peryx.redb"), b"not a redb database").unwrap();

    operator::restore(&backup, target.path(), true, &mut Vec::new()).unwrap();

    let meta = MetaStore::open_existing_read_only(target.path().join("peryx.redb")).unwrap();
    assert_eq!(meta.writer_identity().unwrap().as_deref(), Some("node-a"));
}

#[test]
fn test_restore_preserves_the_recovery_point_and_reports_cost() {
    let (_holder, backup) = identified_backup("node-a", 2);
    let dest = tempfile::tempdir().unwrap();
    let restored = dest.path().join("restored");
    let mut out = Vec::new();

    operator::restore(&backup, &restored, false, &mut out).unwrap();

    let meta = MetaStore::open_existing_read_only(restored.join("peryx.redb")).unwrap();
    assert_eq!(meta.writer_identity().unwrap().as_deref(), Some("node-a"));
    assert!(meta.current_serial().unwrap() >= 2);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("bytes\t"), "{text}");
    assert!(text.contains("elapsed_ms\t"), "{text}");
}
