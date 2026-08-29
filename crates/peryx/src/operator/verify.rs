use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead as _, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use peryx_driver::BlobReferenceScanError;
use peryx_plugin_registry::PluginRegistry;
use peryx_storage::blob::Digest;
use peryx_storage::meta::MetaStore;

use super::{
    BLOB_INDEX_HEADER, BackupCheck, BackupManifest, BlobIndexEntry, HashedFile, ManifestAvailability, ManifestFile,
    backup_blob_relpath, backup_config_with_plugins, backup_plugins, hash_existing_file, read_manifest,
};

/// # Errors
/// Returns an error if verification finds a problem or the manifest cannot be read.
pub fn backup_verify(path: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    backup_verify_with_plugins(path, &crate::compiled_plugins(), out)
}

/// # Errors
/// Returns an error when the backup is unreadable or inconsistent.
pub fn backup_verify_with_plugins(path: &Path, plugins: &PluginRegistry, out: &mut dyn Write) -> anyhow::Result<()> {
    let manifest = read_manifest(path)?;
    let mut problems = 0;
    let config = verify_manifest_file(path, &manifest.config, "config", out, &mut problems)?;
    let plugins = match config {
        ManifestFileCheck::Match => backup_plugins(&backup_config_with_plugins(path, &manifest, plugins)?, plugins)?,
        ManifestFileCheck::Mismatch | ManifestFileCheck::Unscannable => plugins.activate([])?,
    };
    let check = check_backup_contents(path, &manifest, &plugins, out, problems)?;
    if check.problems == 0 {
        writeln!(out, "ok")?;
        Ok(())
    } else {
        writeln!(out, "problems\t{}", check.problems)?;
        bail!("backup verification failed with {} problem(s)", check.problems)
    }
}

pub(super) fn is_missing_file(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|error| error.kind() == std::io::ErrorKind::NotFound)
}

pub(super) fn check_backup_with_plugins(
    path: &Path,
    manifest: &BackupManifest,
    plugins: &PluginRegistry,
    out: &mut dyn Write,
) -> anyhow::Result<BackupCheck> {
    let mut problems = 0;
    verify_manifest_file(path, &manifest.config, "config", out, &mut problems)?;
    check_backup_contents(path, manifest, plugins, out, problems)
}

fn check_backup_contents(
    path: &Path,
    manifest: &BackupManifest,
    plugins: &PluginRegistry,
    out: &mut dyn Write,
    mut problems: u64,
) -> anyhow::Result<BackupCheck> {
    let mut blobs = BTreeMap::new();
    if matches!(
        verify_manifest_file(path, &manifest.blob_index.file, "blob-index", out, &mut problems)?,
        ManifestFileCheck::Match | ManifestFileCheck::Mismatch
    ) {
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
    if verify_metadata_file(path, &manifest.metadata, out, &mut problems)? == ManifestFileCheck::Match {
        match crate::metadata::open_existing_copy(&path.join(&manifest.metadata.path), plugins) {
            Ok(meta) => {
                check_metadata_references(plugins.drivers(), &blobs, &meta, out, &mut problems)?;
                check_availability_state(&manifest.availability, &meta, out, &mut problems)?;
            }
            Err(err) => {
                problems += 1;
                writeln!(out, "problem\tmetadata\t{}\t{err}", manifest.metadata.path)?;
            }
        }
    }
    check_membership(&manifest.availability, out, &mut problems)?;
    Ok(BackupCheck { problems, blobs })
}

/// Re-derive the recovery point from the copied metadata store and reject a manifest that names a
/// different one, catching a metadata file swapped for one taken at another point or edited in place.
fn check_availability_state(
    availability: &ManifestAvailability,
    meta: &MetaStore,
    out: &mut dyn Write,
    problems: &mut u64,
) -> anyhow::Result<()> {
    let frontier = meta.current_serial().context("read backup metadata frontier")?;
    if availability.metadata_frontier != frontier {
        *problems += 1;
        let expected = availability.metadata_frontier;
        writeln!(
            out,
            "problem\tavailability\tfrontier\texpected {expected}, found {frontier}"
        )?;
    }
    let placements = if availability.mode == peryx_ha::AvailabilityMode::None.as_str() {
        0
    } else {
        meta.count_artifact_placements()
            .context("count backup artifact placements")?
    };
    if availability.placements != placements {
        *problems += 1;
        let expected = availability.placements;
        writeln!(
            out,
            "problem\tavailability\tplacements\texpected {expected}, found {placements}"
        )?;
    }
    let identity = meta.writer_identity().context("read backup writer identity")?;
    if availability.writer_identity != identity {
        *problems += 1;
        let expected = availability.writer_identity.as_deref().unwrap_or("none");
        let found = identity.as_deref().unwrap_or("none");
        writeln!(
            out,
            "problem\tavailability\twriter-identity\texpected {expected}, found {found}"
        )?;
    }
    Ok(())
}

/// Reject a datacenter roster the runtime could never consume: an empty group or member set, a
/// duplicated node, datacenter, or address identity, or a member count of writers other than one.
fn check_membership(
    availability: &ManifestAvailability,
    out: &mut dyn Write,
    problems: &mut u64,
) -> anyhow::Result<()> {
    if !matches!(availability.mode.as_str(), "none" | "dc" | "ha") {
        *problems += 1;
        writeln!(out, "problem\tavailability\tmode\tunsupported {}", availability.mode)?;
    }
    let Some(membership) = &availability.membership else {
        return Ok(());
    };
    if membership.group.is_empty() {
        *problems += 1;
        writeln!(out, "problem\tavailability\tmembership\tempty group")?;
    }
    if membership.members.is_empty() {
        *problems += 1;
        writeln!(out, "problem\tavailability\tmembership\tempty roster")?;
    }
    let mut nodes = BTreeSet::new();
    let mut dcs = BTreeSet::new();
    let mut addresses = BTreeSet::new();
    let mut writers = 0_u64;
    for member in &membership.members {
        if !nodes.insert(member.node.as_str()) {
            *problems += 1;
            writeln!(out, "problem\tavailability\tmembership\tduplicate node {}", member.node)?;
        }
        if availability.mode == "ha" && !dcs.insert(member.dc.as_str()) {
            *problems += 1;
            writeln!(out, "problem\tavailability\tmembership\tduplicate dc {}", member.dc)?;
        }
        if !addresses.insert(member.address.as_str()) {
            *problems += 1;
            let address = &member.address;
            writeln!(out, "problem\tavailability\tmembership\tduplicate address {address}")?;
        }
        match member.role.as_str() {
            "writer" => writers += 1,
            "replica" => {}
            role => {
                *problems += 1;
                writeln!(out, "problem\tavailability\tmembership\tinvalid role {role}")?;
            }
        }
    }
    if !membership.members.is_empty() && writers != 1 {
        *problems += 1;
        writeln!(
            out,
            "problem\tavailability\tmembership\texpected one writer, found {writers}"
        )?;
    }
    Ok(())
}

fn verify_manifest_file(
    root: &Path,
    expected: &ManifestFile,
    kind: &str,
    out: &mut dyn Write,
    problems: &mut u64,
) -> anyhow::Result<ManifestFileCheck> {
    let Some(path) = existing_manifest_file(root, expected, kind, out, problems)? else {
        return Ok(ManifestFileCheck::Unscannable);
    };
    let actual = hash_existing_file(&path)?;
    compare_manifest_file(expected, &actual, kind, out, problems)
}

fn verify_metadata_file(
    root: &Path,
    expected: &ManifestFile,
    out: &mut dyn Write,
    problems: &mut u64,
) -> anyhow::Result<ManifestFileCheck> {
    let Some(path) = existing_manifest_file(root, expected, "metadata", out, problems)? else {
        return Ok(ManifestFileCheck::Unscannable);
    };
    let actual = match hash_existing_file(&path) {
        Ok(actual) => actual,
        Err(err) => {
            *problems += 1;
            writeln!(out, "problem\tmetadata\t{}\tI/O error: {err}", expected.path)?;
            return Ok(ManifestFileCheck::Unscannable);
        }
    };
    compare_manifest_file(expected, &actual, "metadata", out, problems)
}

fn existing_manifest_file(
    root: &Path,
    expected: &ManifestFile,
    kind: &str,
    out: &mut dyn Write,
    problems: &mut u64,
) -> anyhow::Result<Option<PathBuf>> {
    let path = root.join(&expected.path);
    if path.is_file() {
        Ok(Some(path))
    } else {
        *problems += 1;
        writeln!(out, "problem\t{kind}\t{}\tmissing", expected.path)?;
        Ok(None)
    }
}

fn compare_manifest_file(
    expected: &ManifestFile,
    actual: &HashedFile,
    kind: &str,
    out: &mut dyn Write,
    problems: &mut u64,
) -> anyhow::Result<ManifestFileCheck> {
    let sha_matches = actual.sha256 == expected.sha256;
    if !sha_matches {
        *problems += 1;
        let path = &expected.path;
        let expected = &expected.sha256;
        let found = &actual.sha256;
        out.write_all(format!("problem\t{kind}\t{path}\tsha256 expected {expected}, found {found}\n").as_bytes())?;
    }
    let size_matches = actual.size_bytes == expected.size_bytes;
    if !size_matches {
        *problems += 1;
        let path = &expected.path;
        let expected = expected.size_bytes;
        let found = actual.size_bytes;
        writeln!(out, "problem\t{kind}\t{path}\tsize expected {expected}, found {found}")?;
    }
    Ok(if sha_matches && size_matches {
        ManifestFileCheck::Match
    } else {
        ManifestFileCheck::Mismatch
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestFileCheck {
    Unscannable,
    Mismatch,
    Match,
}

fn check_metadata_references(
    drivers: &peryx_driver::DriverSet,
    entries: &BTreeMap<String, BlobIndexEntry>,
    meta: &MetaStore,
    out: &mut dyn Write,
    problems: &mut u64,
) -> anyhow::Result<()> {
    let scan = match drivers.scan_blob_references(meta) {
        Ok(scan) => scan,
        Err(BlobReferenceScanError::MissingDrivers(ecosystems)) => {
            *problems += 1;
            writeln!(
                out,
                "problem\tmetadata-reference-scope\t{}\tmissing blob-reference driver",
                ecosystems.iter().map(String::as_str).collect::<Vec<_>>().join(",")
            )?;
            return Ok(());
        }
        Err(error) => return Err(error).context("scan backup metadata blob references"),
    };
    for digest in scan.digests {
        if !entries.contains_key(&digest) {
            *problems += 1;
            writeln!(out, "problem\tblob-index\t{digest}\tmissing referenced digest")?;
        }
    }
    writeln!(out, "scope\tecosystems\t{}", scan.ecosystems.join(","))?;
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
        let expected_path = backup_blob_relpath(&digest);
        if *blob_path != expected_path {
            *problems += 1;
            writeln!(out, "problem\tblob-index\t{}\tinvalid path", digest.as_str())?;
        }
        if entries
            .insert(
                digest.as_str().to_owned(),
                BlobIndexEntry {
                    size_bytes,
                    path: expected_path,
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
