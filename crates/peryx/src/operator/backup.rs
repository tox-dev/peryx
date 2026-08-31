use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use peryx_plugin_registry::PluginRegistry;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;

#[cfg(not(unix))]
use super::is_empty_dir;
use super::snapshot::config_snapshot;
use super::{
    Access, BACKUP_FORMAT, BLOB_INDEX_HEADER, BackupManifest, HashedFile, ManifestAvailability, ManifestBlobIndex,
    ManifestFile, STAGING_PREFIX, backup_blob_relpath, config_availability, copy_hashed_file, create_private_dir_all,
    hash_file, sync_parent, sync_tree, unix_now, write_hashed_file, write_manifest,
};
use crate::config::Config;

#[cfg(test)]
#[path = "../../tests/unit/tests/operator/backup_publish_tests.rs"]
mod backup_publish_tests;

/// # Errors
/// Returns an error if the repository uses an object-store blob backend, the source metadata store
/// is open by a running node, the backup target is not empty, metadata cannot be read, or a
/// referenced blob is missing or mismatched while it is copied.
pub fn backup_create(config: &Config, path: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    backup_create_with_plugins(config, &crate::compiled_plugins(), path, out)
}

/// # Errors
/// Returns an error when the source cannot be quiesced or copied into a complete backup.
pub fn backup_create_with_plugins(
    config: &Config,
    plugins: &PluginRegistry,
    path: &Path,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    crate::app::reject_object_store_blob(config, "creating an offline backup")?;
    let distributed = config.availability.mode().is_distributed();
    let source_metadata = config.data_dir.join("peryx.redb");
    let target = BackupTarget::reserve(path)?;
    let source_meta = quiesce_source(&source_metadata)?;
    let plugins = crate::server::activate_plugins(config, plugins)?;
    let references = plugins
        .drivers()
        .scan_blob_references(&source_meta)
        .context("scan metadata blob references")?;
    let config_snapshot = config_snapshot(config)?;
    let metadata_context = format!("copy metadata store {}", source_metadata.display());
    let (_, metadata_file) = copy_hashed_file(
        &source_metadata,
        target.create_file(Path::new("metadata/peryx.redb"), Access::Private)?,
        "metadata/peryx.redb",
    )
    .context(metadata_context)?;
    let config_info = write_hashed_file(
        target.create_file(Path::new("config.toml"), Access::Private)?,
        config_snapshot.as_bytes(),
        "config.toml",
    )
    .context("write config snapshot")?;

    let source_blobs = BlobStorage::filesystem(config.data_dir.join("blobs"));
    let (blob_count, blob_bytes, metadata_frontier, placements, writer_identity, blob_index_file) = {
        let mut index = BufWriter::new(
            target
                .create_file(Path::new("blobs.tsv"), Access::Shared)
                .context("create blobs.tsv")?,
        );
        writeln!(index, "{BLOB_INDEX_HEADER}")?;
        let (blob_count, blob_bytes) = copy_referenced_blobs(&references.digests, &source_blobs, &target, &mut index)?;
        let index = index.into_inner()?;
        index.sync_all()?;
        let metadata_frontier = source_meta.current_serial().context("read metadata frontier")?;
        let placements = if distributed {
            source_meta
                .count_artifact_placements()
                .context("count artifact placements")?
        } else {
            0
        };
        let writer_identity = source_meta.writer_identity().context("read metadata writer identity")?;
        (
            blob_count,
            blob_bytes,
            metadata_frontier,
            placements,
            writer_identity,
            index,
        )
    };
    let (mode, membership) = config_availability(config);
    let availability = ManifestAvailability {
        mode,
        metadata_frontier,
        placements,
        writer_identity,
        membership,
    };
    let metadata_info = manifest_file(
        "metadata/peryx.redb",
        hash_file(metadata_file).context("hash metadata store")?,
    );
    let blob_index_info = hash_file(blob_index_file).context("hash blobs.tsv")?;
    let manifest = BackupManifest {
        format: BACKUP_FORMAT,
        created_at_unix: unix_now(),
        config: config_info,
        metadata: metadata_info,
        blob_index: ManifestBlobIndex {
            file: manifest_file("blobs.tsv", blob_index_info),
            count: blob_count,
            blob_bytes,
        },
        availability,
    };
    let manifest_file = target.create_file(Path::new("manifest.json"), Access::Private)?;
    write_manifest(manifest_file, &manifest)?;
    target.publish()?;
    writeln!(out, "created\t{}", path.display())?;
    writeln!(out, "metadata\t{}", config.data_dir.join("peryx.redb").display())?;
    writeln!(out, "ecosystems\t{}", references.ecosystems.join(","))?;
    writeln!(out, "blobs\t{blob_count}\t{blob_bytes}")?;
    let mode = &manifest.availability.mode;
    let frontier = manifest.availability.metadata_frontier;
    let placements = manifest.availability.placements;
    writeln!(
        out,
        "availability\t{mode}\tfrontier {frontier}\tplacements {placements}"
    )?;
    Ok(())
}

fn manifest_file(path: &str, hashed: HashedFile) -> ManifestFile {
    ManifestFile {
        path: path.to_owned(),
        sha256: hashed.sha256,
        size_bytes: hashed.size_bytes,
    }
}

/// Hold a read-only handle on the source metadata store so a running node's writer refuses the backup.
///
/// redb grants a read-only open a shared file lock and a writer an exclusive one, so a serving process
/// holding the data directory open makes this fail with `DatabaseAlreadyOpen`; keeping the returned
/// handle alive across the copy also stops a node from starting mid-copy. The config `read_only` flag is
/// not consulted: another process may hold the same directory open under different settings.
fn quiesce_source(source: &Path) -> anyhow::Result<MetaStore> {
    MetaStore::open_existing_read_only(source).map_err(|err| {
        if err.is_database_already_open() {
            anyhow::anyhow!(
                "metadata store {} is open by a running node; stop the node before creating a backup",
                source.display()
            )
        } else {
            anyhow::Error::new(err).context(format!("open metadata store {} read-only", source.display()))
        }
    })
}

/// Copy every blob the metadata store references into the backup, writing one index row per blob and
/// returning the count and total bytes. Blobs are content-addressed artifacts, so they carry the
/// platform-default mode under the owner-only backup root rather than a private one.
fn copy_referenced_blobs(
    digests: &std::collections::BTreeSet<String>,
    source_blobs: &BlobStorage,
    target: &BackupTarget,
    index: &mut impl Write,
) -> anyhow::Result<(u64, u64)> {
    let mut blob_count = 0_u64;
    let mut blob_bytes = 0_u64;
    for digest in digests {
        let digest = Digest::from_hex(digest).context("metadata scan returned an invalid digest")?;
        let source = source_blobs
            .blocking()
            .materialize(&digest)
            .context(format!("referenced blob {} is missing", digest.as_str()))?;
        let relpath = backup_blob_relpath(&digest);
        let copied = copy_hashed_file(
            source.path(),
            target.create_file(Path::new(&relpath), Access::Shared)?,
            &relpath,
        )
        .context(format!("copy referenced blob {}", digest.as_str()))?
        .0;
        if copied.sha256 != digest.as_str() {
            bail!(
                "referenced blob {} hashed as {} while copying",
                digest.as_str(),
                copied.sha256
            );
        }
        blob_count += 1;
        blob_bytes += copied.size_bytes;
        let digest_hex = digest.as_str();
        let size = copied.size_bytes;
        writeln!(index, "{digest_hex}\t{size}\t{relpath}")?;
    }
    Ok((blob_count, blob_bytes))
}

/// A backup under construction in a private randomized sibling of the path it will be published at.
///
/// Nothing is ever written into the final path: the whole tree is built in the sibling and swapped in
/// with a single rename, so a copy, hash, or synchronization failure leaves the final path exactly as
/// the attempt found it — absent, or the empty directory the caller reserved — rather than a partial
/// tree the next attempt would refuse. The sibling shares the final path's parent, which keeps the
/// publishing rename a same-filesystem operation.
#[derive(Debug)]
struct BackupTarget {
    #[cfg(unix)]
    dir: File,
    staging: tempfile::TempDir,
    path: PathBuf,
}

impl BackupTarget {
    /// Refuse an occupied target and reserve a staging sibling to build the backup in.
    ///
    /// The target is inspected first so an occupied one is reported as such rather than as a staging
    /// failure; the reservation is created with create-new semantics, so two concurrent attempts on
    /// one target build in separate trees and only the publishing rename arbitrates between them.
    fn reserve(path: &Path) -> anyhow::Result<Self> {
        let path = std::path::absolute(path).context(format!("resolve backup path {}", path.display()))?;
        Self::inspect_target(&path)?;
        let parent = staging_parent(&path)?;
        create_private_dir_all(parent)?;
        let mut builder = tempfile::Builder::new();
        builder.prefix(STAGING_PREFIX);
        #[cfg(unix)]
        builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let staging = builder
            .tempdir_in(parent)
            .context(format!("create backup staging directory in {}", parent.display()))?;
        Ok(Self {
            #[cfg(unix)]
            dir: open_dir(staging.path())?,
            staging,
            path,
        })
    }

    /// Make the staged tree durable and link it at the final path.
    ///
    /// Directories are flushed from the leaves up so every member's directory entry is durable before
    /// the tree gains a name a reader follows, and the final path's parent is flushed after the rename
    /// so the backup's own name survives a power loss. A rename onto a directory that gained an entry
    /// after it was inspected fails, so a backup another attempt already published is never replaced.
    fn publish(self) -> anyhow::Result<()> {
        let Self { mut staging, path, .. } = self;
        sync_tree(staging.path())?;
        rename_into_place(staging.path(), &path)?;
        // The staged tree now lives at the final path; leaving cleanup armed would aim it at a name
        // another attempt is free to reserve.
        staging.disable_cleanup(true);
        sync_parent(&path)
    }

    #[cfg(unix)]
    fn inspect_target(path: &Path) -> anyhow::Result<()> {
        use rustix::fs::Dir;

        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context(format!("inspect backup path {}", path.display())),
        };
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "backup path {} is a symbolic link",
            path.display()
        );
        anyhow::ensure!(
            metadata.is_dir(),
            "backup path {} exists and is not a directory",
            path.display()
        );
        let dir = open_dir(path)?;
        anyhow::ensure!(
            rustix::fs::fstat(&dir)?.st_uid == rustix::process::geteuid().as_raw(),
            "backup path {} is owned by another user",
            path.display()
        );
        for entry in Dir::read_from(&dir).context(format!("read directory {}", path.display()))? {
            let entry = entry.context(format!("read directory {}", path.display()))?;
            anyhow::ensure!(
                matches!(entry.file_name().to_bytes(), b"." | b".."),
                "backup path {} is not empty",
                path.display()
            );
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn inspect_target(path: &Path) -> anyhow::Result<()> {
        if !path.exists() {
            return Ok(());
        }
        anyhow::ensure!(
            path.is_dir(),
            "backup path {} exists and is not a directory",
            path.display()
        );
        anyhow::ensure!(is_empty_dir(path)?, "backup path {} is not empty", path.display());
        Ok(())
    }

    #[cfg(unix)]
    fn create_file(&self, path: &Path, access: Access) -> anyhow::Result<File> {
        use rustix::fs::{Mode, OFlags};

        let parent = self.open_parent(path)?;
        let name = path
            .file_name()
            .context(format!("backup member {} has no file name", path.display()))?;
        let mode = match access {
            Access::Private => Mode::from_raw_mode(0o600),
            Access::Shared => Mode::from_raw_mode(0o666),
        };
        Ok(File::from(
            rustix::fs::openat(
                &parent,
                name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                mode,
            )
            .context(format!("create backup member {}", path.display()))?,
        ))
    }

    #[cfg(not(unix))]
    fn create_file(&self, path: &Path, access: Access) -> anyhow::Result<File> {
        let path = self.staging.path().join(path);
        let parent = path
            .parent()
            .context(format!("backup member {} has no parent", path.display()))?;
        std::fs::create_dir_all(parent).context(format!("create {}", parent.display()))?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        let _ = access;
        options
            .open(&path)
            .context(format!("create backup member {}", path.display()))
    }

    #[cfg(unix)]
    fn open_parent(&self, path: &Path) -> anyhow::Result<File> {
        use rustix::fs::{Mode, OFlags};

        let mut parent = self.dir.try_clone()?;
        let path = path.parent().context("backup members always carry a file name")?;
        for name in path {
            match rustix::fs::mkdirat(&parent, name, Mode::RWXU) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error).context(format!("create backup directory {}", path.display())),
            }
            parent = File::from(
                rustix::fs::openat(
                    &parent,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .context(format!("open backup directory {}", path.display()))?,
            );
        }
        Ok(parent)
    }
}

/// Name the directory the staging sibling is reserved in. A filesystem root holds no sibling of
/// itself, so it surfaces a structured error rather than an unwrap.
fn staging_parent(path: &Path) -> anyhow::Result<&Path> {
    path.parent()
        .context(format!("backup path {} has no parent directory", path.display()))
}

#[cfg(unix)]
fn open_dir(path: &Path) -> anyhow::Result<File> {
    use rustix::fs::{Mode, OFlags};

    Ok(File::from(
        rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context(format!("open backup directory {}", path.display()))?,
    ))
}

/// Rename replaces an empty directory and refuses a populated one, so the caller-supplied empty target
/// is handed over in one step while a backup another attempt completed there stays untouched.
#[cfg(unix)]
fn rename_into_place(staging: &Path, path: &Path) -> anyhow::Result<()> {
    std::fs::rename(staging, path).context(format!("publish backup to {}", path.display()))
}

/// Windows refuses a rename onto an existing directory, so the caller-supplied empty target is removed
/// first and the staged tree moved to an absent destination. `remove_dir` refuses a directory holding
/// entries, so a backup another attempt completed there is never cleared; an interruption between the
/// two steps leaves the target absent, which the next attempt reserves from scratch.
#[cfg(not(unix))]
fn rename_into_place(staging: &Path, path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        std::fs::remove_dir(path).context(format!("clear reserved backup target {}", path.display()))?;
    }
    std::fs::rename(staging, path).context(format!("publish backup to {}", path.display()))
}
