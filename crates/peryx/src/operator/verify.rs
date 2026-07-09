//! Backup verification: manifest, blob index, and metadata reference checks.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead as _, BufReader, Write};
use std::path::Path;

use anyhow::{Context as _, bail};
use peryx_storage::blob::Digest;
use peryx_storage::meta::MetaStore;

use super::{
    BLOB_INDEX_HEADER, BackupCheck, BackupManifest, BlobIndexEntry, HashedFile, ManifestFile, backup_blob_relpath,
    hash_existing_file, read_manifest,
};

/// Verify a backup directory.
///
/// # Errors
/// Returns an error if verification finds a problem or the manifest cannot be read.
pub fn backup_verify(path: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let manifest = read_manifest(path)?;
    let check = check_backup(path, &manifest, out)?;
    if check.problems == 0 {
        writeln!(out, "ok")?;
        Ok(())
    } else {
        writeln!(out, "problems\t{}", check.problems)?;
        bail!("backup verification failed with {} problem(s)", check.problems)
    }
}

pub(super) fn check_backup(path: &Path, manifest: &BackupManifest, out: &mut dyn Write) -> anyhow::Result<BackupCheck> {
    let mut problems = 0;
    verify_manifest_file(path, &manifest.config, "config", out, &mut problems)?;
    let mut blobs = BTreeMap::new();
    if verify_manifest_file(path, &manifest.blob_index.file, "blob-index", out, &mut problems)?.is_some() {
        blobs = read_blob_index(path.join(&manifest.blob_index.file.path).as_path(), out, &mut problems)?;
        let indexed_bytes = blobs.values().map(|entry| entry.size_bytes).sum::<u64>();
        if blobs.len() as u64 != manifest.blob_index.count {
            problems += 1;
            let expected = manifest.blob_index.count;
            let found = blobs.len();
            writeln!(out, "problem\tblob-index\tcount\texpected {expected}, found {found}")?;
        }
        if indexed_bytes != manifest.blob_index.blob_bytes {
            problems += 1;
            let expected = manifest.blob_index.blob_bytes;
            let message = format!("problem\tblob-index\tbytes\texpected {expected}, found {indexed_bytes}\n");
            out.write_all(message.as_bytes())?;
        }
        for (digest, entry) in &blobs {
            verify_blob(path, digest, entry, out, &mut problems)?;
        }
    }
    let metadata_path = path.join(&manifest.metadata.path);
    if metadata_path.is_file() {
        match MetaStore::open_existing(path.join(&manifest.metadata.path)) {
            Ok(meta) => check_metadata_references(&blobs, &meta, out, &mut problems)?,
            Err(err) => {
                problems += 1;
                writeln!(out, "problem\tmetadata\t{}\t{err}", manifest.metadata.path)?;
            }
        }
    } else {
        problems += 1;
        writeln!(out, "problem\tmetadata\t{}\tmissing", manifest.metadata.path)?;
    }
    Ok(BackupCheck { problems, blobs })
}

fn verify_manifest_file(
    root: &Path,
    expected: &ManifestFile,
    kind: &str,
    out: &mut dyn Write,
    problems: &mut u64,
) -> anyhow::Result<Option<HashedFile>> {
    let path = root.join(&expected.path);
    if !path.is_file() {
        *problems += 1;
        writeln!(out, "problem\t{kind}\t{}\tmissing", expected.path)?;
        return Ok(None);
    }
    let actual = hash_existing_file(&path)?;
    if actual.sha256 != expected.sha256 {
        *problems += 1;
        let path = &expected.path;
        let expected = &expected.sha256;
        let found = &actual.sha256;
        out.write_all(format!("problem\t{kind}\t{path}\tsha256 expected {expected}, found {found}\n").as_bytes())?;
    }
    if actual.size_bytes != expected.size_bytes {
        *problems += 1;
        let path = &expected.path;
        let expected = expected.size_bytes;
        let found = actual.size_bytes;
        writeln!(out, "problem\t{kind}\t{path}\tsize expected {expected}, found {found}")?;
    }
    Ok(Some(actual))
}

fn check_metadata_references(
    entries: &BTreeMap<String, BlobIndexEntry>,
    meta: &MetaStore,
    out: &mut dyn Write,
    problems: &mut u64,
) -> anyhow::Result<()> {
    for digest in crate::app::referenced_blob_digests(meta).context("scan backup metadata blob references")? {
        if !entries.contains_key(&digest) {
            *problems += 1;
            writeln!(out, "problem\tblob-index\t{digest}\tmissing referenced digest")?;
        }
    }
    Ok(())
}

fn read_blob_index(
    path: &Path,
    out: &mut dyn Write,
    problems: &mut u64,
) -> anyhow::Result<BTreeMap<String, BlobIndexEntry>> {
    let mut entries = BTreeMap::new();
    let file = File::open(path).context(format!("open {}", path.display()))?;
    for (line_number, line) in BufReader::new(file).lines().enumerate() {
        let line = line.context(format!("read {} line {}", path.display(), line_number + 1))?;
        if line_number == 0 {
            if line != BLOB_INDEX_HEADER {
                *problems += 1;
                writeln!(out, "problem\tblob-index\theader\tinvalid header")?;
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let parts = line.split('\t').collect::<Vec<_>>();
        let [digest, size_bytes, blob_path] = parts.as_slice() else {
            *problems += 1;
            writeln!(out, "problem\tblob-index\tline {}\tinvalid row", line_number + 1)?;
            continue;
        };
        let Some(digest) = Digest::from_hex(digest) else {
            *problems += 1;
            writeln!(out, "problem\tblob-index\tline {}\tinvalid digest", line_number + 1)?;
            continue;
        };
        let Ok(size_bytes) = size_bytes.parse::<u64>() else {
            *problems += 1;
            writeln!(out, "problem\tblob-index\t{}\tinvalid size", digest.as_str())?;
            continue;
        };
        if *blob_path != backup_blob_relpath(&digest) {
            *problems += 1;
            writeln!(out, "problem\tblob-index\t{}\tinvalid path", digest.as_str())?;
        }
        if entries
            .insert(
                digest.as_str().to_owned(),
                BlobIndexEntry {
                    size_bytes,
                    path: (*blob_path).to_owned(),
                },
            )
            .is_some()
        {
            *problems += 1;
            writeln!(out, "problem\tblob-index\tline {}\tduplicate digest", line_number + 1)?;
        }
    }
    Ok(entries)
}

fn verify_blob(
    root: &Path,
    digest: &str,
    entry: &BlobIndexEntry,
    out: &mut dyn Write,
    problems: &mut u64,
) -> anyhow::Result<()> {
    let path = root.join(&entry.path);
    if !path.is_file() {
        *problems += 1;
        writeln!(out, "problem\tblob\t{digest}\tmissing")?;
        return Ok(());
    }
    let actual = hash_existing_file(&path)?;
    if actual.size_bytes != entry.size_bytes {
        *problems += 1;
        let expected = entry.size_bytes;
        let found = actual.size_bytes;
        writeln!(out, "problem\tblob\t{digest}\tsize expected {expected}, found {found}")?;
    }
    if actual.sha256 != digest {
        *problems += 1;
        let actual = actual.sha256;
        writeln!(out, "problem\tblob\t{digest}\tsha256 expected {digest}, found {actual}")?;
    }
    Ok(())
}
