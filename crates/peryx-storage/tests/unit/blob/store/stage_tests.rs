#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use super::discard_stage;
use super::remove_pending;
#[cfg(unix)]
use super::remove_pending_with;

#[test]
fn test_remove_pending_treats_a_missing_stage_as_removed() {
    let dir = tempfile::tempdir().unwrap();
    assert!(remove_pending(&dir.path().join("absent")).is_ok());
}

#[cfg(unix)]
#[test]
fn test_remove_pending_backs_off_then_reports_a_persistent_denial() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let target = locked.join("stage");
    std::fs::write(&target, b"x").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

    // Record the backoff schedule instead of sleeping, so the retry stays deterministic and fast.
    let mut waits = Vec::new();
    let result = remove_pending_with(&target, |backoff| waits.push(backoff));

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(result.unwrap_err().kind(), crate::blob::BlobErrorKind::Io);
    assert_eq!(
        waits,
        [1, 2, 4, 8, 16, 32].map(Duration::from_millis),
        "a persistent denial doubles the backoff up to the 64ms ceiling"
    );
}

#[cfg(unix)]
#[test]
fn test_discard_stage_falls_back_when_rename_is_denied() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let (_file, path) = tempfile::NamedTempFile::new_in(&locked).unwrap().into_parts();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = discard_stage(path);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(result.unwrap_err().kind(), crate::blob::BlobErrorKind::Io);
}
