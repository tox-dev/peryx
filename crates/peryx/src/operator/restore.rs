//! Restoring a verified backup into a data directory.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, bail};
use peryx_storage::blob::Digest;
use peryx_storage::meta::MetaStore;

use super::verify::check_backup;
use super::{Access, BackupCheck, BackupManifest, backup_blob_path, copy_hashed, is_empty_dir, read_manifest};
use crate::config::{self, Config};

#[cfg(test)]
#[path = "../../tests/unit/operator/restore_publish_tests.rs"]
mod restore_publish_tests;

/// Restore a backup into a data directory.
///
/// # Errors
/// Returns an error if the backup fails verification, the target is unsafe, or files cannot be
/// copied.
pub fn restore(backup: &Path, data_dir: &Path, force: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let started = Instant::now();
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
    guard_target_identity(&manifest, data_dir, out)?;
    guard_target(backup, data_dir, force)?;
    let staging = staging_path(data_dir)?;
    if let Err(err) = stage_backup(backup, &manifest, check, &staging).and_then(|()| publish(&staging, data_dir)) {
        let _ = remove_any(&staging);
        return Err(err);
    }
    writeln!(out, "restored\t{}", data_dir.display())?;
    let count = manifest.blob_index.count;
    let blob_bytes = manifest.blob_index.blob_bytes;
    writeln!(out, "blobs\t{count}\t{blob_bytes}")?;
    let bytes = restored_bytes(&manifest);
    let elapsed_ms = started.elapsed().as_millis();
    writeln!(out, "bytes\t{bytes}")?;
    writeln!(out, "elapsed_ms\t{elapsed_ms}")?;
    Ok(())
}

/// The total bytes a restore reads from the backup: the metadata snapshot, the config snapshot, the blob
/// index, and every referenced blob. An operator reads it against the elapsed time to size the recovery.
const fn restored_bytes(manifest: &BackupManifest) -> u64 {
    manifest
        .metadata
        .size_bytes
        .saturating_add(manifest.config.size_bytes)
        .saturating_add(manifest.blob_index.file.size_bytes)
        .saturating_add(manifest.blob_index.blob_bytes)
}

/// Refuse a restore that would adopt one node's recovery point under a different node's identity, and
/// warn when it would roll a node back over control state it has already advanced past.
///
/// A target with no metadata store is a fresh recovery and passes. When the target already holds one,
/// its claimed writer identity must match the backup's: restoring node `b`'s state onto node `a`'s
/// directory would give two nodes the same identity, a split brain no `--force` should wave through, so
/// this rejects it regardless of `force`. A same-identity target sitting at a control serial ahead of
/// the backup is a genuine rollback; that is the operator's call under `--force`, so it warns rather than
/// refuses. The prepared-directory step still enforces the empty-target rule for the non-forced path.
///
/// # Errors
/// Returns an error when the target belongs to a different node, or its identity or serial cannot be
/// read.
fn guard_target_identity(manifest: &BackupManifest, data_dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let target = data_dir.join("peryx.redb");
    if !target.is_file() {
        return Ok(());
    }
    let Ok(meta) = MetaStore::open_existing_read_only(&target) else {
        return Ok(());
    };
    let existing = meta.writer_identity().context("read restore target writer identity")?;
    if let (Some(backup), Some(existing)) = (manifest.availability.writer_identity.as_deref(), existing.as_deref())
        && backup != existing
    {
        bail!(
            "refusing to restore node {backup} onto a directory claimed by node {existing}; \
             clear the target or restore {existing}'s own backup"
        );
    }
    let target_serial = meta.current_serial().context("read restore target control serial")?;
    let frontier = manifest.availability.metadata_frontier;
    if target_serial > frontier {
        writeln!(
            out,
            "warning\trestore\trollback\ttarget at serial {target_serial}, backup at {frontier}"
        )?;
    }
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

/// Refuse a restore whose backup aliases the target, and enforce the empty-target rule without
/// `--force`. A restore never touches the target directly: it stages a full copy in a sibling directory
/// and swaps it in, so the removal the forced path used to do up front is gone.
///
/// # Errors
/// Returns an error when the backup path is the target, or the non-forced target exists and is not an
/// empty directory.
fn guard_target(backup: &Path, data_dir: &Path, force: bool) -> anyhow::Result<()> {
    if paths_alias(backup, data_dir) {
        bail!("refusing to restore backup {} onto itself", data_dir.display());
    }
    if !data_dir.exists() {
        return Ok(());
    }
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
    } else if !force {
        bail!(
            "restore target {} exists and is not a directory; pass --force to replace it",
            data_dir.display()
        );
    }
    Ok(())
}

/// Whether two paths name the same location. Both are canonicalized so a symlink, `.`, or `..` cannot
/// smuggle the backup in as the target; a path that does not yet exist keeps its literal form.
fn paths_alias(left: &Path, right: &Path) -> bool {
    let resolve = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolve(left) == resolve(right)
}

/// Copy the verified backup into a clean staging directory and make its contents durable, so publication
/// swaps in a complete tree rather than writing into the live target.
///
/// # Errors
/// Returns an error if the staging directory cannot be reset or any file cannot be copied.
fn stage_backup(backup: &Path, manifest: &BackupManifest, check: BackupCheck, staging: &Path) -> anyhow::Result<()> {
    reset_staging(staging)?;
    copy_hashed(
        &backup.join(&manifest.metadata.path),
        &staging.join("peryx.redb"),
        "peryx.redb",
        Access::Private,
    )
    .context("restore metadata store")?;
    copy_hashed(
        &backup.join(&manifest.config.path),
        &staging.join("config.toml"),
        "config.toml",
        Access::Private,
    )
    .context("restore config snapshot")?;
    for (digest, entry) in check.blobs {
        let digest = Digest::from_hex(&digest).context("backup blob index contained an invalid digest")?;
        copy_hashed(
            &backup.join(&entry.path),
            &backup_blob_path(staging, &digest),
            &entry.path,
            Access::Shared,
        )
        .context(format!("restore blob {}", digest.as_str()))?;
    }
    sync_tree(staging);
    Ok(())
}

fn reset_staging(staging: &Path) -> anyhow::Result<()> {
    remove_any(staging)?;
    std::fs::create_dir_all(staging).context(format!("create staging directory {}", staging.display()))
}

/// Swap the staged directory into the target so the target is only ever the complete prior state or the
/// complete restored state. An existing target is renamed aside first and restored if the swap fails; it
/// is removed only once the replacement is in place and its parent is durable.
///
/// # Errors
/// Returns an error if the target cannot be moved aside or the staged directory cannot be renamed in.
fn publish(staging: &Path, data_dir: &Path) -> anyhow::Result<()> {
    if !data_dir.exists() {
        std::fs::rename(staging, data_dir).context(format!("publish restored data to {}", data_dir.display()))?;
        sync_parent(data_dir);
        return Ok(());
    }
    let aside = aside_path(data_dir)?;
    remove_any(&aside)?;
    std::fs::rename(data_dir, &aside).context(format!("move existing restore target {} aside", data_dir.display()))?;
    match std::fs::rename(staging, data_dir) {
        Ok(()) => {
            sync_parent(data_dir);
            let _ = remove_any(&aside);
            Ok(())
        }
        Err(err) => {
            let _ = std::fs::rename(&aside, data_dir);
            Err(err).context(format!("publish restored data to {}", data_dir.display()))
        }
    }
}

fn staging_path(data_dir: &Path) -> anyhow::Result<PathBuf> {
    sibling_path(data_dir, ".restore-staging")
}

fn aside_path(data_dir: &Path) -> anyhow::Result<PathBuf> {
    sibling_path(data_dir, ".restore-old")
}

/// A sibling of the target sharing its parent, so a rename between them is an atomic same-filesystem
/// swap.
///
/// # Errors
/// Returns an error when the target has no final path component to hang the suffix on.
fn sibling_path(data_dir: &Path, suffix: &str) -> anyhow::Result<PathBuf> {
    let mut name = data_dir
        .file_name()
        .with_context(|| format!("restore target {} has no final path component", data_dir.display()))?
        .to_owned();
    name.push(suffix);
    Ok(data_dir.with_file_name(name))
}

fn remove_any(path: &Path) -> anyhow::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path).context(format!("remove {}", path.display()))
    } else if path.exists() {
        std::fs::remove_file(path).context(format!("remove {}", path.display()))
    } else {
        Ok(())
    }
}

/// Flush a directory subtree so the file names a later rename publishes survive a crash. Durability is a
/// best-effort hint: the all-or-nothing guarantee comes from the rename ordering, not from the flush, so
/// a sync error never fails an otherwise complete restore.
fn sync_tree(path: &Path) {
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                sync_tree(&child);
            }
        }
    }
    sync_dir(path);
}

fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        sync_dir(parent);
    }
}

#[cfg(unix)]
fn sync_dir(path: &Path) {
    if let Ok(dir) = std::fs::File::open(path) {
        let _ = dir.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) {}
