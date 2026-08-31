use std::path::Path;

use super::{BackupTarget, STAGING_PREFIX, staging_parent};

#[test]
fn test_reserve_stages_a_private_sibling_and_leaves_the_target_alone() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("backup");

    let target = BackupTarget::reserve(&path).unwrap();

    let staging = target.staging.path();
    let name = staging.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(
        (staging.parent(), name.starts_with(STAGING_PREFIX), path.exists()),
        (Some(root.path()), true, false)
    );
}

#[test]
fn test_reserve_reports_an_unresolvable_backup_path() {
    let error = BackupTarget::reserve(Path::new("")).unwrap_err();

    assert_eq!(error.to_string(), "resolve backup path ");
}

#[test]
fn test_staging_parent_reports_a_backup_path_without_a_parent() {
    let error = staging_parent(Path::new("/")).unwrap_err();

    assert_eq!(error.to_string(), "backup path / has no parent directory");
}

#[cfg(unix)]
#[test]
fn test_reserve_reports_a_parent_it_cannot_create() {
    let root = read_only_dir();
    let parent = root.path().join("nested");

    let error = BackupTarget::reserve(&parent.join("backup")).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!("create backup directory {}", parent.display())
    );
}

#[cfg(unix)]
#[test]
fn test_reserve_reports_a_parent_it_cannot_stage_in() {
    let root = read_only_dir();

    let error = BackupTarget::reserve(&root.path().join("backup")).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!("create backup staging directory in {}", root.path().display())
    );
}

#[test]
fn test_publish_refuses_a_target_populated_after_it_was_reserved() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("backup");
    let target = BackupTarget::reserve(&path).unwrap();
    let staging = target.staging.path().to_owned();
    std::fs::write(staging.join("manifest.json"), b"staged").unwrap();
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("manifest.json"), b"published").unwrap();

    let error = target.publish().unwrap_err();

    assert_eq!(
        (
            error.to_string(),
            std::fs::read(path.join("manifest.json")).unwrap(),
            staging.exists(),
        ),
        (
            format!("publish backup to {}", path.display()),
            b"published".to_vec(),
            false,
        )
    );
}

#[test]
fn test_publish_swaps_the_staged_tree_into_a_reserved_target() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("backup");
    std::fs::create_dir(&path).unwrap();
    let target = BackupTarget::reserve(&path).unwrap();
    let staging = target.staging.path().to_owned();
    std::fs::create_dir(staging.join("metadata")).unwrap();
    std::fs::write(staging.join("metadata").join("peryx.redb"), b"staged").unwrap();

    target.publish().unwrap();

    assert_eq!(
        (
            std::fs::read(path.join("metadata").join("peryx.redb")).unwrap(),
            staging.exists(),
        ),
        (b"staged".to_vec(), false)
    );
}

/// A directory the effective user may traverse but not write, so a staging reservation below it fails
/// the way a permission-denied parent does in the field.
#[cfg(unix)]
fn read_only_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o500))
        .tempdir()
        .unwrap();
    assert_eq!(
        std::fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
        0o500
    );
    root
}
