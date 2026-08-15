use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use nix::sys::stat::{Mode, umask};

use super::{backup_fixture, valid_backup};
use crate::operator;

static UMASK_LOCK: Mutex<()> = Mutex::new(());

// The process-global mask requires serial access.
struct LooseUmask {
    _guard: MutexGuard<'static, ()>,
    previous: Mode,
}

impl LooseUmask {
    fn set() -> Self {
        let guard = UMASK_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = umask(Mode::from_bits_truncate(0o022));
        Self {
            _guard: guard,
            previous,
        }
    }
}

impl Drop for LooseUmask {
    fn drop(&mut self) {
        umask(self.previous);
    }
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn test_backup_create_writes_owner_only_files_under_a_loose_umask() {
    let _umask = LooseUmask::set();
    let (_source, config, _content, _metadata) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");

    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();

    assert_eq!(mode_of(&backup), 0o700);
    assert_eq!(mode_of(&backup.join("config.toml")), 0o600);
    assert_eq!(mode_of(&backup.join("metadata/peryx.redb")), 0o600);
    assert_eq!(mode_of(&backup.join("manifest.json")), 0o600);
}

#[test]
fn test_backup_create_tightens_a_preexisting_group_readable_target() {
    let _umask = LooseUmask::set();
    let (_source, config, _content, _metadata) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    std::fs::create_dir(&backup).unwrap();
    std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o755)).unwrap();

    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();

    assert_eq!(mode_of(&backup), 0o700);
    assert_eq!(mode_of(&backup.join("manifest.json")), 0o600);
}

#[test]
fn test_restore_writes_owner_only_config_and_metadata_under_a_loose_umask() {
    let _umask = LooseUmask::set();
    let (_source, root, _config, backup, _content, _metadata) = valid_backup();
    let data_dir = root.path().join("restored");

    operator::restore(&backup, &data_dir, false, &mut Vec::new()).unwrap();

    assert_eq!(mode_of(&data_dir.join("config.toml")), 0o600);
    assert_eq!(mode_of(&data_dir.join("peryx.redb")), 0o600);
}
