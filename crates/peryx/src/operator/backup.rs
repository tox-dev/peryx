use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context as _, bail};
use peryx_plugin_registry::PluginRegistry;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;

use super::snapshot::config_snapshot;
use super::{
    Access, BACKUP_FORMAT, BLOB_INDEX_HEADER, BackupManifest, ManifestAvailability, ManifestBlobIndex, ManifestFile,
    backup_blob_path, backup_blob_relpath, config_availability, copy_hashed, create_private_dir_all,
    hash_existing_file, is_empty_dir, tighten_private_dir, unix_now, write_hashed, write_manifest,
};
use crate::config::Config;

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
    validate_backup_target(path)?;
    let source_meta = quiesce_source(&source_metadata)?;
    let plugins = crate::server::activate_plugins(config, plugins)?;
    let references = plugins
        .drivers()
        .scan_blob_references(&source_meta)
        .context("scan metadata blob references")?;
    prepare_new_backup_dir(path)?;
    let config_info = write_hashed(
        &path.join("config.toml"),
        config_snapshot(config)?.as_bytes(),
        "config.toml",
    )
    .context("write config snapshot")?;
    let metadata_context = format!("copy metadata store {}", source_metadata.display());
    copy_hashed(
        &source_metadata,
        &path.join("metadata/peryx.redb"),
        "metadata/peryx.redb",
        Access::Private,
    )
    .context(metadata_context)?;

    let source_blobs = BlobStorage::filesystem(config.data_dir.join("blobs"));
    let (blob_count, blob_bytes, metadata_frontier, placements, writer_identity) = {
        let meta = crate::metadata::open_existing(&path.join("metadata/peryx.redb"), &plugins)?;
        let mut index = BufWriter::new(File::create(path.join("blobs.tsv")).context("create blobs.tsv")?);
        writeln!(index, "{BLOB_INDEX_HEADER}")?;
        let (blob_count, blob_bytes) = copy_referenced_blobs(&references.digests, &source_blobs, path, &mut index)?;
        index.into_inner()?.sync_all()?;
        let metadata_frontier = meta.current_serial().context("read metadata frontier")?;
        let placements = if distributed {
            meta.count_artifact_placements().context("count artifact placements")?
        } else {
            0
        };
        let writer_identity = meta.writer_identity().context("read metadata writer identity")?;
        (blob_count, blob_bytes, metadata_frontier, placements, writer_identity)
    };
    let (mode, membership) = config_availability(config);
    let availability = ManifestAvailability {
        mode,
        metadata_frontier,
        placements,
        writer_identity,
        membership,
    };
    let metadata_info = {
        let hashed = hash_existing_file(&path.join("metadata/peryx.redb")).context("hash metadata store")?;
        ManifestFile {
            path: "metadata/peryx.redb".to_owned(),
            sha256: hashed.sha256,
            size_bytes: hashed.size_bytes,
        }
    };
    let blob_index_info = hash_existing_file(&path.join("blobs.tsv")).context("hash blobs.tsv")?;
    let manifest = BackupManifest {
        format: BACKUP_FORMAT,
        created_at_unix: unix_now(),
        config: config_info,
        metadata: metadata_info,
        blob_index: ManifestBlobIndex {
            file: ManifestFile {
                path: "blobs.tsv".to_owned(),
                sha256: blob_index_info.sha256,
                size_bytes: blob_index_info.size_bytes,
            },
            count: blob_count,
            blob_bytes,
        },
        availability,
    };
    write_manifest(path, &manifest)?;
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
    path: &Path,
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
        let copied = copy_hashed(
            source.path(),
            &backup_blob_path(path, &digest),
            &relpath,
            Access::Shared,
        )
        .context(format!("copy referenced blob {}", digest.as_str()))?;
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

fn prepare_new_backup_dir(path: &Path) -> anyhow::Result<()> {
    validate_backup_target(path)?;
    if path.exists() {
        return tighten_private_dir(path);
    }
    create_private_dir_all(path)
}

fn validate_backup_target(path: &Path) -> anyhow::Result<()> {
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
