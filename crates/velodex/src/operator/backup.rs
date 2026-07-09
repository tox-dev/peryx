//! Full offline backup creation.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context as _, bail};
use velodex_storage::blob::{BlobStore, Digest};
use velodex_storage::meta::MetaStore;

use super::snapshot::config_snapshot;
use super::{
    BACKUP_FORMAT, BLOB_INDEX_HEADER, BackupManifest, ManifestBlobIndex, ManifestFile, backup_blob_path,
    backup_blob_relpath, copy_hashed, hash_existing_file, is_empty_dir, unix_now, write_hashed, write_manifest,
};
use crate::config::Config;

/// Create a full offline backup of a data directory.
///
/// # Errors
/// Returns an error if the backup target is not empty, metadata cannot be read, or a referenced
/// blob is missing or mismatched while it is copied.
pub fn backup_create(config: &Config, path: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    prepare_new_backup_dir(path)?;
    let config_info = write_hashed(
        &path.join("config.toml"),
        config_snapshot(config)?.as_bytes(),
        "config.toml",
    )
    .context("write config snapshot")?;
    let metadata_context = format!("copy metadata store {}", config.data_dir.join("velodex.redb").display());
    copy_hashed(
        &config.data_dir.join("velodex.redb"),
        &path.join("metadata/velodex.redb"),
        "metadata/velodex.redb",
    )
    .context(metadata_context)?;

    let source_blobs = BlobStore::new(config.data_dir.join("blobs"));
    let mut blob_count = 0_u64;
    let mut blob_bytes = 0_u64;
    {
        let meta =
            MetaStore::open_existing(path.join("metadata/velodex.redb")).context("open copied metadata store")?;
        let mut index = BufWriter::new(File::create(path.join("blobs.tsv")).context("create blobs.tsv")?);
        writeln!(index, "{BLOB_INDEX_HEADER}")?;
        for digest in crate::app::referenced_blob_digests(&meta).context("scan metadata blob references")? {
            let digest = Digest::from_hex(&digest).context("metadata scan returned an invalid digest")?;
            let source = source_blobs.path_for(&digest);
            if !source.is_file() {
                bail!(
                    "referenced blob {} is missing from {}",
                    digest.as_str(),
                    source.display()
                );
            }
            let backup_path = backup_blob_path(path, &digest);
            let copied = copy_hashed(&source, &backup_path, &backup_blob_relpath(&digest))
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
            let relpath = backup_blob_relpath(&digest);
            writeln!(index, "{digest_hex}\t{size}\t{relpath}")?;
        }
        index.into_inner()?.sync_all()?;
    }
    let metadata_info = {
        let hashed = hash_existing_file(&path.join("metadata/velodex.redb")).context("hash metadata store")?;
        ManifestFile {
            path: "metadata/velodex.redb".to_owned(),
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
    };
    write_manifest(path, &manifest)?;
    writeln!(out, "created\t{}", path.display())?;
    writeln!(out, "metadata\t{}", config.data_dir.join("velodex.redb").display())?;
    writeln!(out, "blobs\t{blob_count}\t{blob_bytes}")?;
    Ok(())
}

fn prepare_new_backup_dir(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        anyhow::ensure!(
            path.is_dir(),
            "backup path {} exists and is not a directory",
            path.display()
        );
        anyhow::ensure!(is_empty_dir(path)?, "backup path {} is not empty", path.display());
    }
    std::fs::create_dir_all(path).context(format!("create backup directory {}", path.display()))
}
