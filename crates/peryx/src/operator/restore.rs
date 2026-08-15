use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, bail};
use peryx_storage::blob::Digest;

use super::verify::{check_backup_with_plugins, is_missing_file};
use super::{
    Access, BackupCheck, BackupManifest, backup_blob_path, backup_config_with_plugins, backup_plugins, copy_hashed,
    is_empty_dir, read_manifest,
};
use crate::config::Config;

#[cfg(test)]
#[path = "../../tests/unit/tests/operator/restore_publish_tests.rs"]
mod restore_publish_tests;

/// # Errors
/// Returns an error if the backup fails verification, the target is unsafe, or files cannot be
/// copied.
pub fn restore(backup: &Path, data_dir: &Path, force: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    restore_with_plugins(backup, data_dir, force, &crate::compiled_plugins(), out)
}

/// # Errors
/// Returns an error if verification, snapshot parsing, target guards, or publication fails.
pub fn restore_with_plugins(
    backup: &Path,
    data_dir: &Path,
    force: bool,
    plugins: &peryx_plugin_registry::PluginRegistry,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let manifest = read_manifest(backup)?;
    let backup_config = match backup_config_with_plugins(backup, &manifest, plugins) {
        Ok(config) => config,
        Err(error) if is_missing_file(&error) => {
            let mut verification = Vec::new();
            let check = check_backup_with_plugins(backup, &manifest, &plugins.activate([])?, &mut verification)?;
            bail!(
                "backup verification failed with {problems} problem(s): {}",
                String::from_utf8_lossy(&verification),
                problems = check.problems,
            );
        }
        Err(error) => return Err(error),
    };
    let plugins = backup_plugins(&backup_config, plugins)?;
    let mut verification = Vec::new();
    let check = check_backup_with_plugins(backup, &manifest, &plugins, &mut verification)?;
    if check.problems != 0 {
        bail!(
            "backup verification failed with {problems} problem(s): {}",
            String::from_utf8_lossy(&verification),
            problems = check.problems,
        );
    }
    warn_config_mismatch(&backup_config, data_dir, out)?;
    guard_target_identity(&manifest, data_dir, &plugins, out)?;
    guard_target(backup, data_dir, force)?;
    let staging = staging_path(data_dir)?;
    let result = match stage_backup(backup, &manifest, check, &staging) {
        Ok(()) => crate::metadata::open_existing(&staging.join("peryx.redb"), &plugins).and_then(|store| {
            drop(store);
            publish(&staging, data_dir)
        }),
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        return Err(cleanup_restore_failure(&staging, error));
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
fn guard_target_identity(
    manifest: &BackupManifest,
    data_dir: &Path,
    plugins: &peryx_plugin_registry::PluginRegistry,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let target = data_dir.join("peryx.redb");
    if !target.is_file() {
        return Ok(());
    }
    let meta = crate::metadata::open_existing_read_only(&target, plugins)?;
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

fn warn_config_mismatch(backup_config: &Config, data_dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    if backup_config.data_dir == data_dir {
        return Ok(());
    }
    let backup_dir = backup_config.data_dir.display();
    let restore_dir = data_dir.display();
    let message = format!("warning\tconfig\tdata_dir\tbackup={backup_dir}\trestore={restore_dir}\n");
    out.write_all(message.as_bytes())?;
    Ok(())
}

/// Refuse overlap between the backup, target, and restore work paths. Without `--force`, the target
/// must be absent or an empty directory.
///
/// # Errors
/// Returns an error when the backup path is the target, or the non-forced target exists and is not an
/// empty directory.
fn guard_target(backup: &Path, data_dir: &Path, force: bool) -> anyhow::Result<()> {
    let backup = resolve_path(backup)?;
    let target = resolve_path(data_dir)?;
    let staging = resolve_path(&staging_path(data_dir)?)?;
    let aside = resolve_path(&aside_path(data_dir)?)?;
    if backup == target {
        bail!("refusing to restore backup {} onto itself", data_dir.display());
    }
    if paths_overlap(&backup, &target) || paths_overlap(&backup, &staging) || paths_overlap(&backup, &aside) {
        bail!(
            "refusing to restore from backup {} because it overlaps restore target {} or its work paths",
            backup.display(),
            data_dir.display()
        );
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

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Resolve symlinks in the existing prefix while retaining missing path components.
fn resolve_path(path: &Path) -> anyhow::Result<PathBuf> {
    let absolute = std::path::absolute(path).context(format!("make path {} absolute", path.display()))?;
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .context(format!("path {} has no existing ancestor", path.display()))?;
        missing.push(name.to_owned());
        existing = existing
            .parent()
            .context(format!("path {} has no existing ancestor", path.display()))?;
    }
    let mut resolved = std::fs::canonicalize(existing).context(format!("resolve path {}", path.display()))?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

/// Copy the verified backup into a clean staging directory and make its contents durable, so publication
/// swaps in a complete tree rather than writing into the live target.
///
/// # Errors
/// Returns an error if the staging directory cannot be reset or any file cannot be copied.
fn stage_backup(backup: &Path, manifest: &BackupManifest, check: BackupCheck, staging: &Path) -> anyhow::Result<()> {
    reset_staging(staging)?;
    let metadata = copy_hashed(
        &backup.join(&manifest.metadata.path),
        &staging.join("peryx.redb"),
        "peryx.redb",
        Access::Private,
    )
    .context("restore metadata store")?;
    ensure_copy_matches(&metadata, &manifest.metadata, "metadata")?;
    let config = copy_hashed(
        &backup.join(&manifest.config.path),
        &staging.join("config.toml"),
        "config.toml",
        Access::Private,
    )
    .context("restore config snapshot")?;
    ensure_copy_matches(&config, &manifest.config, "config")?;
    for (digest, entry) in check.blobs {
        let digest = Digest::from_hex(&digest).context("backup blob index contained an invalid digest")?;
        let copied = copy_hashed(
            &backup.join(&entry.path),
            &backup_blob_path(staging, &digest),
            &entry.path,
            Access::Shared,
        )
        .context(format!("restore blob {}", digest.as_str()))?;
        ensure_blob_copy_matches(&copied, &digest, entry.size_bytes)?;
    }
    sync_tree(staging)?;
    Ok(())
}

fn ensure_copy_matches(actual: &super::ManifestFile, expected: &super::ManifestFile, kind: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        actual.sha256 == expected.sha256 && actual.size_bytes == expected.size_bytes,
        "backup {kind} changed after verification"
    );
    Ok(())
}

fn ensure_blob_copy_matches(actual: &super::ManifestFile, digest: &Digest, size_bytes: u64) -> anyhow::Result<()> {
    anyhow::ensure!(
        actual.sha256 == digest.as_str() && actual.size_bytes == size_bytes,
        "backup blob {} changed after verification",
        digest.as_str()
    );
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
        sync_parent(data_dir)?;
        return Ok(());
    }
    let aside = aside_path(data_dir)?;
    remove_any(&aside)?;
    std::fs::rename(data_dir, &aside).context(format!("move existing restore target {} aside", data_dir.display()))?;
    match std::fs::rename(staging, data_dir) {
        Ok(()) => {
            sync_parent(data_dir)?;
            remove_any(&aside)?;
            Ok(())
        }
        Err(err) => {
            let publish = anyhow::Error::new(err).context(format!("publish restored data to {}", data_dir.display()));
            Err(rollback_publish(&aside, data_dir, publish))
        }
    }
}

fn rollback_publish(aside: &Path, data_dir: &Path, publish: anyhow::Error) -> anyhow::Error {
    match std::fs::rename(aside, data_dir) {
        Ok(()) => publish,
        Err(rollback) => publish.context(format!(
            "restore original target {} after publish failure: {rollback}",
            data_dir.display()
        )),
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
        .context(format!(
            "restore target {} has no final path component",
            data_dir.display()
        ))?
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

fn cleanup_restore_failure(staging: &Path, error: anyhow::Error) -> anyhow::Error {
    match remove_any(staging) {
        Ok(()) => error,
        Err(cleanup) => error.context(format!(
            "clean restore staging {} after failure: {cleanup}",
            staging.display()
        )),
    }
}

/// Flush the staged directory tree before publication.
fn sync_tree(path: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(path).context(format!("read restored directory {}", path.display()))? {
        let child = entry
            .context(format!("read restored directory entry in {}", path.display()))?
            .path();
        if child.is_dir() {
            sync_tree(&child)?;
        }
    }
    sync_dir(path)
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context(format!("restored target {} has no parent", path.display()))?;
    sync_dir(parent)
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::File::open(path)
        .context(format!("open directory {} for sync", path.display()))?
        .sync_all()
        .context(format!("sync directory {}", path.display()))
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}
