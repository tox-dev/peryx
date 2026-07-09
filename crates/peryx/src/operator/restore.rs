//! Restoring a verified backup into a data directory.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use peryx_storage::blob::Digest;

use super::verify::check_backup;
use super::{BackupManifest, backup_blob_path, copy_hashed, is_empty_dir, read_manifest};
use crate::config::{self, Config};

/// Restore a backup into a data directory.
///
/// # Errors
/// Returns an error if the backup fails verification, the target is unsafe, or files cannot be
/// copied.
pub fn restore(backup: &Path, data_dir: &Path, force: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let manifest = read_manifest(backup)?;
    let mut verification = Vec::new();
    let check = check_backup(backup, &manifest, &mut verification)?;
    if check.problems != 0 {
        bail!(
            "backup verification failed with {problems} problem(s): {}",
            String::from_utf8_lossy(&verification),
            problems = check.problems,
        );
    }
    warn_config_mismatch(backup, &manifest, data_dir, out)?;
    prepare_restore_dir(data_dir, force)?;
    copy_hashed(
        &backup.join(&manifest.metadata.path),
        &data_dir.join("peryx.redb"),
        "peryx.redb",
    )
    .context("restore metadata store")?;
    copy_hashed(
        &backup.join(&manifest.config.path),
        &data_dir.join("config.toml"),
        "config.toml",
    )
    .context("restore config snapshot")?;
    for (digest, entry) in check.blobs {
        let digest = Digest::from_hex(&digest).context("backup blob index contained an invalid digest")?;
        copy_hashed(
            &backup.join(&entry.path),
            &backup_blob_path(data_dir, &digest),
            &entry.path,
        )
        .context(format!("restore blob {}", digest.as_str()))?;
    }
    writeln!(out, "restored\t{}", data_dir.display())?;
    let count = manifest.blob_index.count;
    let bytes = manifest.blob_index.blob_bytes;
    writeln!(out, "blobs\t{count}\t{bytes}")?;
    Ok(())
}

fn warn_config_mismatch(
    backup: &Path,
    manifest: &BackupManifest,
    data_dir: &Path,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(backup.join(&manifest.config.path))
        .context(format!("read backup config {}", manifest.config.path))?;
    let backup_config = Config::default()
        .apply(config::from_toml(PathBuf::from(&manifest.config.path), &text)?)
        .context("parse backup config snapshot")?;
    if backup_config.data_dir == data_dir {
        return Ok(());
    }
    let backup_dir = backup_config.data_dir.display();
    let restore_dir = data_dir.display();
    let message = format!("warning\tconfig\tdata_dir\tbackup={backup_dir}\trestore={restore_dir}\n");
    out.write_all(message.as_bytes())?;
    Ok(())
}

fn prepare_restore_dir(data_dir: &Path, force: bool) -> anyhow::Result<()> {
    if data_dir.exists() {
        if data_dir.is_dir() {
            if is_empty_dir(data_dir)? {
                return Ok(());
            }
            if !force {
                bail!(
                    "restore target {} is not empty; pass --force to replace it",
                    data_dir.display()
                );
            }
            std::fs::remove_dir_all(data_dir).context(format!("remove {}", data_dir.display()))?;
        } else {
            if !force {
                bail!(
                    "restore target {} exists and is not a directory; pass --force to replace it",
                    data_dir.display()
                );
            }
            std::fs::remove_file(data_dir).context(format!("remove {}", data_dir.display()))?;
        }
    }
    std::fs::create_dir_all(data_dir).context(format!("create restore target {}", data_dir.display()))
}
