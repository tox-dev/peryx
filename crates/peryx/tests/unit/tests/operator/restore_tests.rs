#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use super::support::{
    backup_create_with_references, backup_fixture, backup_verify, blob_relpath, claimed_data_dir, identified_backup,
    mutate_manifest, resign_file, restore, valid_backup,
};
use crate::config::{AvailabilityConfig, DcMember, DcMembership, DcRole, ReplicationConfig, SecretSource};
use crate::operator;

#[test]
fn test_restore_public_api_rejects_an_unsupported_manifest() {
    let fixture = valid_backup();
    mutate_manifest(&fixture.backup, |manifest| {
        manifest["format"] = serde_json::json!(3);
    });

    let error = operator::restore(
        &fixture.backup,
        &fixture.root.path().join("restored"),
        false,
        &mut Vec::new(),
    )
    .unwrap_err();

    assert_eq!(error.to_string(), "unsupported backup format 3");
}

#[rstest]
#[case::directory(true, "not empty")]
#[case::file(false, "exists and is not a directory")]
fn test_restore_requires_force_for_occupied_targets(#[case] directory: bool, #[case] expected: &str) {
    let fixture = valid_backup();
    let restored = fixture.root.path().join("restored");
    if directory {
        std::fs::create_dir(&restored).unwrap();
        std::fs::write(restored.join("blocker"), b"x").unwrap();
    } else {
        std::fs::write(&restored, b"x").unwrap();
    }

    let error = restore(&fixture.backup, &restored, false, &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains(expected), "{error:#}");
}

#[rstest]
#[case::directory(true)]
#[case::file(false)]
fn test_restore_force_replaces_occupied_targets(#[case] directory: bool) {
    let fixture = valid_backup();
    let restored = fixture.root.path().join("restored");
    if directory {
        std::fs::create_dir(&restored).unwrap();
        std::fs::write(restored.join("blocker"), b"x").unwrap();
    } else {
        std::fs::write(&restored, b"x").unwrap();
    }

    restore(&fixture.backup, &restored, true, &mut Vec::new()).unwrap();

    assert!(restored.join("peryx.redb").is_file());
    assert!(!restored.join("blocker").exists());
}

#[test]
fn test_restore_accepts_an_empty_precreated_target() {
    let fixture = valid_backup();
    let restored = fixture.root.path().join("restored");
    std::fs::create_dir(&restored).unwrap();

    restore(&fixture.backup, &restored, false, &mut Vec::new()).unwrap();

    assert!(restored.join("peryx.redb").is_file());
}

#[test]
fn test_restore_accepts_a_dc_backup_with_a_shared_datacenter() {
    let (source, mut config, _, _) = backup_fixture();
    config.availability = AvailabilityConfig::Dc(ReplicationConfig::Primary {
        source: "primary-a".to_owned(),
        token: SecretSource::Literal("token".to_owned()),
    });
    config.dc_membership = Some(DcMembership {
        group: "group-a".to_owned(),
        members: vec![
            DcMember {
                node: "node-a".to_owned(),
                dc: "east".to_owned(),
                address: "https://a:1".to_owned(),
                role: DcRole::Writer,
            },
            DcMember {
                node: "node-b".to_owned(),
                dc: "east".to_owned(),
                address: "https://b:1".to_owned(),
                role: DcRole::Replica,
            },
        ],
    });
    config.validate().unwrap();
    let backup = source.path().join("backup");
    backup_create_with_references(&config, &backup, &mut Vec::new()).unwrap();
    backup_verify(&backup, &mut Vec::new()).unwrap();
    let restored = source.path().join("restored");

    restore(&backup, &restored, false, &mut Vec::new()).unwrap();

    assert!(restored.join("peryx.redb").is_file());
}

#[test]
fn test_restore_rejects_verification_failures() {
    let fixture = valid_backup();
    std::fs::remove_file(fixture.backup.join(blob_relpath(&fixture.content_digest))).unwrap();

    let error = restore(
        &fixture.backup,
        &fixture.root.path().join("restored"),
        false,
        &mut Vec::new(),
    )
    .unwrap_err();

    assert_eq!(
        (
            error.to_string().contains("backup verification failed"),
            error.to_string().contains("problem\tblob"),
        ),
        (true, true)
    );
}

#[test]
fn test_restore_reports_a_missing_snapshotted_config_with_all_verification_problems() {
    let fixture = valid_backup();
    std::fs::remove_file(fixture.backup.join("config.toml")).unwrap();

    let error = restore(
        &fixture.backup,
        &fixture.root.path().join("restored"),
        false,
        &mut Vec::new(),
    )
    .unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("backup verification failed with 1 problem(s)"),
        "{message}"
    );
    assert!(message.contains("problem\tconfig\tconfig.toml\tmissing"), "{message}");
}

#[test]
fn test_restore_round_trip_restores_metadata_blobs_and_reports_cost() {
    let fixture = valid_backup();
    let restored = fixture.root.path().join("restored");
    let mut out = Vec::new();

    restore(&fixture.backup, &restored, false, &mut out).unwrap();

    let meta = MetaStore::open_existing(restored.join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(restored.join("blobs"));
    let text = String::from_utf8(out).unwrap();
    let expected_content = b"artifact bytes";
    let expected_metadata = b"metadata bytes";
    let blob_bytes = (expected_content.len() + expected_metadata.len()) as u64;
    let expected_bytes = ["metadata/peryx.redb", "config.toml", "blobs.tsv"]
        .map(|path| std::fs::metadata(fixture.backup.join(path)).unwrap().len())
        .into_iter()
        .sum::<u64>()
        + blob_bytes;
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(
        (
            meta.writer_identity().unwrap(),
            blobs.read(&fixture.content_digest).unwrap(),
            blobs.read(&fixture.metadata_digest).unwrap(),
        ),
        (None, expected_content.to_vec(), expected_metadata.to_vec())
    );
    assert_eq!(
        &lines[..4],
        [
            format!(
                "warning\tconfig\tdata_dir\tbackup={}\trestore={}",
                fixture.config.data_dir.display(),
                restored.display()
            ),
            format!("restored\t{}", restored.display()),
            format!("blobs\t2\t{blob_bytes}"),
            format!("bytes\t{expected_bytes}"),
        ]
    );
    assert!(lines[4].strip_prefix("elapsed_ms\t").unwrap().parse::<u128>().is_ok());
}

#[test]
fn test_restore_omits_the_config_warning_for_the_original_path() {
    let fixture = valid_backup();
    let mut out = Vec::new();

    let error = restore(&fixture.backup, &fixture.config.data_dir, false, &mut out).unwrap_err();

    assert_eq!(
        (
            error.to_string().contains("not empty"),
            String::from_utf8(out).unwrap().contains("warning\tconfig\tdata_dir"),
        ),
        (true, false)
    );
}

#[test]
fn test_restore_rejects_an_aliased_target() {
    let fixture = valid_backup();

    let error = restore(&fixture.backup, &fixture.backup, true, &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("onto itself"), "{error:#}");
}

#[test]
fn test_restore_rejects_a_parentless_target() {
    let fixture = valid_backup();

    let error = restore(&fixture.backup, Path::new("/"), true, &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("no final path component"), "{error:#}");
}

#[test]
fn test_restore_rejects_a_target_containing_the_backup() {
    let fixture = valid_backup();

    let error = restore(&fixture.backup, fixture.root.path(), true, &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("overlaps restore target"), "{error:#}");
    assert!(fixture.backup.join("manifest.json").is_file());
}

#[test]
fn test_restore_rejects_a_backup_at_the_staging_path() {
    let fixture = valid_backup();
    let target = fixture.root.path().join("restored");
    let backup = fixture.root.path().join("restored.restore-staging");
    std::fs::rename(&fixture.backup, &backup).unwrap();

    let error = restore(&backup, &target, true, &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("work paths"), "{error:#}");
    assert!(backup.join("manifest.json").is_file());
}

#[cfg(unix)]
#[test]
fn test_restore_cleans_staging_after_copy_failure() {
    let fixture = valid_backup();
    let restored = fixture.root.path().join("restored");
    std::fs::create_dir(&restored).unwrap();
    std::fs::write(restored.join("keep"), b"old").unwrap();
    let previous = std::fs::metadata(fixture.root.path()).unwrap().permissions().mode();
    std::fs::set_permissions(fixture.root.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

    let result = restore(&fixture.backup, &restored, true, &mut Vec::new());
    std::fs::set_permissions(fixture.root.path(), std::fs::Permissions::from_mode(previous)).unwrap();

    assert_eq!(
        (
            result.unwrap_err().to_string().contains("staging directory"),
            std::fs::read(restored.join("keep")).unwrap(),
            fixture.root.path().join("restored.restore-staging").exists(),
        ),
        (true, b"old".to_vec(), false)
    );
}

#[test]
fn test_restore_rejects_a_target_claimed_by_another_node() {
    let (_holder, backup) = identified_backup("node-a", 1);
    let target = claimed_data_dir("node-b", 1);

    let error = restore(&backup, target.path(), true, &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("refusing to restore node node-a"), "{error}");
    let meta = MetaStore::open_existing_read_only(target.path().join("peryx.redb")).unwrap();
    assert_eq!(meta.writer_identity().unwrap().as_deref(), Some("node-b"));
}

#[rstest]
#[case::rollback(1, 3, true)]
#[case::forward(2, 0, false)]
fn test_restore_same_node_reports_only_rollbacks(
    #[case] backup_mutations: usize,
    #[case] target_mutations: usize,
    #[case] warns: bool,
) {
    let (_holder, backup) = identified_backup("node-a", backup_mutations);
    let target = claimed_data_dir("node-a", target_mutations);
    let mut out = Vec::new();

    restore(&backup, target.path(), true, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    let meta = MetaStore::open_existing_read_only(target.path().join("peryx.redb")).unwrap();
    assert_eq!(
        (
            text.contains("warning\trestore\trollback"),
            meta.writer_identity().unwrap(),
        ),
        (warns, Some("node-a".to_owned()))
    );
}

#[test]
fn test_restore_rejects_an_unreadable_target_store() {
    let (_holder, backup) = identified_backup("node-a", 1);
    let target = tempfile::tempdir().unwrap();
    std::fs::write(target.path().join("peryx.redb"), b"not a redb database").unwrap();

    let error = restore(&backup, target.path(), true, &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("open metadata store"), "{error:#}");
    assert_eq!(
        std::fs::read(target.path().join("peryx.redb")).unwrap(),
        b"not a redb database"
    );
}

#[test]
fn test_restore_rejects_a_live_target_store() {
    let (_holder, backup) = identified_backup("node-a", 1);
    let target = claimed_data_dir("node-a", 1);
    let live = MetaStore::open_existing(target.path().join("peryx.redb")).unwrap();

    let error = restore(&backup, target.path(), true, &mut Vec::new()).unwrap_err();
    drop(live);

    assert!(error.to_string().contains("open metadata store"), "{error:#}");
    let meta = MetaStore::open_existing_read_only(target.path().join("peryx.redb")).unwrap();
    assert_eq!(meta.writer_identity().unwrap().as_deref(), Some("node-a"));
}

#[test]
fn test_restore_reports_invalid_snapshotted_config() {
    let fixture = valid_backup();
    std::fs::write(fixture.backup.join("config.toml"), b"not = [valid").unwrap();
    resign_file(&fixture.backup, "config", "config.toml");

    let error = restore(
        &fixture.backup,
        &fixture.root.path().join("restored"),
        false,
        &mut Vec::new(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("parse"), "{error:#}");
}

#[test]
fn test_restore_propagates_warning_output_errors() {
    let fixture = valid_backup();
    let mut out: &mut [u8] = &mut [];

    let error = restore(&fixture.backup, &fixture.root.path().join("restored"), false, &mut out).unwrap_err();

    assert_eq!(
        error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
        Some(std::io::ErrorKind::WriteZero)
    );
}

#[test]
fn test_restore_propagates_rollback_warning_output_errors() {
    let (holder, backup) = identified_backup("node-a", 1);
    let target = holder.path().join("data");
    let meta = MetaStore::open_existing(target.join("peryx.redb")).unwrap();
    for _ in 0..3 {
        meta.next_serial().unwrap();
    }
    drop(meta);
    let mut out: &mut [u8] = &mut [];

    let error = restore(&backup, &target, true, &mut out).unwrap_err();

    assert_eq!(
        error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
        Some(std::io::ErrorKind::WriteZero)
    );
}

#[test]
fn test_restore_replaces_stale_staging_and_aside_paths() {
    let fixture = valid_backup();
    let restored = fixture.root.path().join("restored");
    std::fs::create_dir(&restored).unwrap();
    std::fs::write(restored.join("old"), b"old").unwrap();
    std::fs::write(fixture.root.path().join("restored.restore-staging"), b"stale").unwrap();
    std::fs::create_dir(fixture.root.path().join("restored.restore-old")).unwrap();

    restore(&fixture.backup, &restored, true, &mut Vec::new()).unwrap();

    assert_eq!(
        (
            restored.join("peryx.redb").is_file(),
            fixture.root.path().join("restored.restore-staging").exists(),
            fixture.root.path().join("restored.restore-old").exists(),
        ),
        (true, false, false)
    );
}
