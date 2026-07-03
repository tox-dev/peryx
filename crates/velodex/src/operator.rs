//! Operator workflows for offline state movement and local artifact import.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead as _, BufReader, BufWriter, Read as _, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use blake2::Blake2bVar;
use blake2::digest::{Update as _, VariableOutput as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use velodex_core::pypi::{DistributionFilename, DistributionFilenameError, parse_distribution_filename};
use velodex_http::upload::{self, StagedUpload, UploadError, UploadForm};
use velodex_storage::blob::{BlobStore, Digest};
use velodex_storage::meta::MetaStore;

use crate::config::{self, Config, IndexKind, LogFormat, LogSink};

const BACKUP_FORMAT: u32 = 1;
const BUFFER_BYTES: usize = 1024 * 1024;
const BLOB_INDEX_HEADER: &str = "sha256\tsize_bytes\tpath";

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
        &data_dir.join("velodex.redb"),
        "velodex.redb",
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

/// Import local wheel and sdist files into a hosted repository.
///
/// # Errors
/// Returns an error if the data directory cannot be opened, the repository cannot accept imported
/// files, or output fails.
pub fn import_dir(config: &Config, repo: &str, dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    if !dir.is_dir() {
        bail!("import directory {} does not exist", dir.display());
    }
    let target = import_target(config, repo)?;
    std::fs::create_dir_all(&config.data_dir)
        .context(format!("create data directory {}", config.data_dir.display()))?;
    let open_context = format!("open metadata store {}", config.data_dir.join("velodex.redb").display());
    let meta = MetaStore::open(config.data_dir.join("velodex.redb")).context(open_context)?;
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    let mut counts = ImportCounts::default();
    writeln!(out, "status\tfilename\tproject\tversion\treason")?;
    walk_files(dir, &mut |path| {
        import_file(dir, path, &target, &meta, &blobs, &mut counts, out)?;
        Ok(())
    })?;
    let imported = counts.imported;
    let skipped = counts.skipped;
    let rejected = counts.rejected;
    out.write_all(format!("summary\t\t\t\timported={imported} skipped={skipped} rejected={rejected}\n").as_bytes())?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct BackupManifest {
    format: u32,
    created_at_unix: i64,
    config: ManifestFile,
    metadata: ManifestFile,
    blob_index: ManifestBlobIndex,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestBlobIndex {
    #[serde(flatten)]
    file: ManifestFile,
    count: u64,
    blob_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestFile {
    path: String,
    sha256: String,
    size_bytes: u64,
}

#[derive(Debug, Clone)]
struct HashedFile {
    sha256: String,
    size_bytes: u64,
}

#[derive(Debug)]
struct BlobIndexEntry {
    size_bytes: u64,
    path: String,
}

struct BackupCheck {
    problems: u64,
    blobs: BTreeMap<String, BlobIndexEntry>,
}

#[derive(Debug)]
struct ImportTarget {
    name: String,
    route: String,
}

#[derive(Default)]
struct ImportCounts {
    imported: u64,
    skipped: u64,
    rejected: u64,
}

#[derive(Serialize)]
struct SnapshotConfig {
    host: String,
    port: u16,
    data_dir: String,
    cache_ttl_secs: i64,
    index: Vec<SnapshotIndex>,
    log: SnapshotLog,
}

#[derive(Serialize)]
struct SnapshotIndex {
    name: String,
    route: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mirror: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_concurrency: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volatile: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    layers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload: Option<String>,
}

#[derive(Serialize)]
struct SnapshotLog {
    level: String,
    format: &'static str,
    sink: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
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

fn write_manifest(path: &Path, manifest: &BackupManifest) -> anyhow::Result<()> {
    let manifest_path = path.join("manifest.json");
    let mut file = File::create(&manifest_path).context(format!("create {}", manifest_path.display()))?;
    serde_json::to_writer_pretty(&mut file, manifest)?;
    writeln!(file)?;
    file.sync_all()?;
    Ok(())
}

fn read_manifest(path: &Path) -> anyhow::Result<BackupManifest> {
    let manifest_path = path.join("manifest.json");
    let manifest: BackupManifest =
        serde_json::from_reader(File::open(&manifest_path).context(format!("open {}", manifest_path.display()))?)
            .context(format!("parse {}", manifest_path.display()))?;
    if manifest.format != BACKUP_FORMAT {
        bail!("unsupported backup format {}", manifest.format);
    }
    Ok(manifest)
}

fn check_backup(path: &Path, manifest: &BackupManifest, out: &mut dyn Write) -> anyhow::Result<BackupCheck> {
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

fn import_target(config: &Config, repo: &str) -> anyhow::Result<ImportTarget> {
    let indexes = crate::server::build_indexes(&config.indexes)?;
    let position = indexes
        .iter()
        .position(|index| index.name == repo)
        .or_else(|| indexes.iter().position(|index| index.route == repo))
        .context(format!("unknown repository {repo:?}"))?;
    let index = &indexes[position];
    match &index.kind {
        velodex_http::IndexKind::Local { .. } => Ok(ImportTarget {
            name: index.name.clone(),
            route: index.route.clone(),
        }),
        velodex_http::IndexKind::Overlay {
            upload: Some(upload), ..
        } => {
            let target = &indexes[*upload];
            Ok(ImportTarget {
                name: target.name.clone(),
                route: index.route.clone(),
            })
        }
        velodex_http::IndexKind::Overlay { upload: None, .. } => {
            bail!("repository {repo:?} has no local upload target")
        }
        velodex_http::IndexKind::Mirror(_) => bail!("repository {repo:?} is read-only"),
    }
}

fn walk_files(dir: &Path, visit: &mut impl FnMut(&Path) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let mut entries = std::fs::read_dir(dir)
        .context(format!("read directory {}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_files(&entry.path(), visit)?;
            continue;
        }
        file_type.is_file().then(|| visit(&entry.path())).transpose()?;
    }
    Ok(())
}

fn import_file(
    root: &Path,
    path: &Path,
    target: &ImportTarget,
    meta: &MetaStore,
    blobs: &BlobStore,
    counts: &mut ImportCounts,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let display = path.strip_prefix(root).unwrap_or(path).display().to_string();
    let filename = path
        .file_name()
        .expect("directory walk only visits named entries")
        .to_string_lossy();
    let parsed = match parse_distribution_filename(&filename) {
        Ok(parsed) => parsed,
        Err(DistributionFilenameError::UnsupportedExtension | DistributionFilenameError::LegacyEgg) => {
            counts.skipped += 1;
            writeln!(out, "skipped\t{display}\t\t\tunsupported file type")?;
            return Ok(());
        }
        Err(err) => {
            counts.rejected += 1;
            writeln!(out, "rejected\t{display}\t\t\tinvalid distribution filename: {err:?}")?;
            return Ok(());
        }
    };
    let staged = stage_file(path, blobs)?;
    let version = parsed.version.to_string();
    match upload::prepare(
        upload_form(&filename, &parsed, &staged),
        staged,
        &target.route,
        unix_now(),
    ) {
        Ok(prepared) => match upload::store_prepared(meta, blobs, &target.name, prepared) {
            Ok(true) => {
                counts.imported += 1;
                let normalized = &parsed.normalized_name;
                writeln!(out, "imported\t{display}\t{normalized}\t{version}\tstored")?;
            }
            Ok(false) => {
                counts.skipped += 1;
                let normalized = &parsed.normalized_name;
                writeln!(out, "skipped\t{display}\t{normalized}\t{version}\talready present")?;
            }
            Err(err) => {
                counts.rejected += 1;
                writeln!(out, "rejected\t{display}\t{}\t{version}\t{err}", parsed.normalized_name)?;
            }
        },
        Err(err) => {
            counts.rejected += 1;
            let normalized = &parsed.normalized_name;
            let reason = upload_error_reason(&err);
            writeln!(out, "rejected\t{display}\t{normalized}\t{version}\t{reason}")?;
        }
    }
    Ok(())
}

fn stage_file(path: &Path, blobs: &BlobStore) -> anyhow::Result<StagedUpload> {
    let mut input = BufReader::with_capacity(BUFFER_BYTES, File::open(path)?);
    let mut pending = blobs.begin()?;
    let mut blake2 = Blake2bVar::new(32).expect("blake2b-256 output size is valid");
    let mut buffer = vec![0; BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        blake2.update(chunk);
        pending.write(chunk)?;
    }
    Ok(StagedUpload {
        blob: pending.finish()?,
        blake2_256: finalize_blake2(blake2),
    })
}

fn upload_form(filename: &str, parsed: &DistributionFilename, staged: &StagedUpload) -> UploadForm {
    UploadForm {
        action: Some("file_upload".to_owned()),
        name: Some(parsed.name.clone()),
        version: Some(parsed.version.to_string()),
        filetype: Some(parsed.kind.upload_filetype().to_owned()),
        sha256_digest: Some(staged.blob.digest().as_str().to_owned()),
        blake2_256_digest: Some(staged.blake2_256.clone()),
        filename: Some(filename.to_owned()),
        ..UploadForm::default()
    }
}

fn upload_error_reason(err: &UploadError) -> String {
    match err {
        UploadError::InvalidContent(message) => format!("invalid content: {message}"),
        UploadError::InvalidMetadataUtf8 => "metadata is not UTF-8".to_owned(),
        UploadError::InvalidRequiresPython(value) => format!("invalid Requires-Python: {value}"),
        UploadError::MetadataNameMismatch { metadata, form } => {
            format!("metadata name {metadata:?} does not match {form:?}")
        }
        UploadError::MetadataVersionMismatch { metadata, form } => {
            format!("metadata version {metadata:?} does not match {form:?}")
        }
        UploadError::MetadataFieldMismatch { field, metadata, form } => {
            format!("metadata field {field} is {metadata:?}, expected {form:?}")
        }
        err => format!("{err:?}"),
    }
}

fn copy_hashed(source: &Path, dest: &Path, manifest_path: &str) -> anyhow::Result<ManifestFile> {
    let parent = dest.parent().expect("hashed files are written below a directory");
    std::fs::create_dir_all(parent).context(format!("create {}", parent.display()))?;
    let mut input = BufReader::with_capacity(BUFFER_BYTES, File::open(source)?);
    let mut output = BufWriter::with_capacity(BUFFER_BYTES, File::create(dest)?);
    let mut hasher = Sha256::new();
    let mut size_bytes = 0;
    let mut buffer = vec![0; BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        size_bytes += read as u64;
    }
    output.into_inner()?.sync_all()?;
    Ok(ManifestFile {
        path: manifest_path.to_owned(),
        sha256: hex(&hasher.finalize()),
        size_bytes,
    })
}

fn write_hashed(path: &Path, bytes: &[u8], manifest_path: &str) -> anyhow::Result<ManifestFile> {
    let parent = path.parent().expect("hashed files are written below a directory");
    std::fs::create_dir_all(parent).context(format!("create {}", parent.display()))?;
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(ManifestFile {
        path: manifest_path.to_owned(),
        sha256: hex(&Sha256::digest(bytes)),
        size_bytes: bytes.len() as u64,
    })
}

fn hash_existing_file(path: &Path) -> anyhow::Result<HashedFile> {
    let mut input = BufReader::with_capacity(BUFFER_BYTES, File::open(path)?);
    let mut hasher = Sha256::new();
    let mut size_bytes = 0;
    let mut buffer = vec![0; BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size_bytes += read as u64;
    }
    Ok(HashedFile {
        sha256: hex(&hasher.finalize()),
        size_bytes,
    })
}

fn backup_blob_path(root: &Path, digest: &Digest) -> PathBuf {
    root.join(backup_blob_relpath(digest))
}

fn backup_blob_relpath(digest: &Digest) -> String {
    let hex = digest.as_str();
    format!("blobs/sha256/{}/{}/{}", &hex[0..2], &hex[2..4], hex)
}

fn is_empty_dir(path: &Path) -> anyhow::Result<bool> {
    Ok(std::fs::read_dir(path)
        .context(format!("read directory {}", path.display()))?
        .next()
        .is_none())
}

fn config_snapshot(config: &Config) -> anyhow::Result<String> {
    let snapshot = SnapshotConfig {
        host: config.host.clone(),
        port: config.port,
        data_dir: config.data_dir.display().to_string(),
        cache_ttl_secs: config.cache_ttl_secs,
        index: config.indexes.iter().map(snapshot_index).collect(),
        log: SnapshotLog {
            level: config.log.level.clone(),
            format: log_format(config.log.format),
            sink: log_sink(config.log.sink),
            file: config.log.file.as_ref().map(|path| path.display().to_string()),
        },
    };
    Ok(toml::to_string_pretty(&snapshot)?)
}

fn snapshot_index(index: &crate::config::IndexConfig) -> SnapshotIndex {
    match &index.kind {
        IndexKind::Mirror {
            upstream,
            username,
            password,
            token,
            upstream_concurrency,
        } => SnapshotIndex {
            name: index.name.clone(),
            route: index.route.clone(),
            mirror: Some(upstream.clone()),
            username: username.clone(),
            password: password.clone(),
            token: token.clone(),
            upstream_concurrency: Some(*upstream_concurrency),
            local: None,
            upload_token: None,
            volatile: None,
            layers: None,
            upload: None,
        },
        IndexKind::Local { upload_token, volatile } => SnapshotIndex {
            name: index.name.clone(),
            route: index.route.clone(),
            mirror: None,
            username: None,
            password: None,
            token: None,
            upstream_concurrency: None,
            local: Some(true),
            upload_token: upload_token.clone(),
            volatile: Some(*volatile),
            layers: None,
            upload: None,
        },
        IndexKind::Overlay { layers, upload } => SnapshotIndex {
            name: index.name.clone(),
            route: index.route.clone(),
            mirror: None,
            username: None,
            password: None,
            token: None,
            upstream_concurrency: None,
            local: None,
            upload_token: None,
            volatile: None,
            layers: Some(layers.clone()),
            upload: upload.clone(),
        },
    }
}

const fn log_format(format: LogFormat) -> &'static str {
    match format {
        LogFormat::Pretty => "pretty",
        LogFormat::Json => "json",
    }
}

const fn log_sink(sink: LogSink) -> &'static str {
    match sink {
        LogSink::Stdout => "stdout",
        LogSink::File => "file",
        LogSink::Journald => "journald",
        LogSink::Syslog => "syslog",
    }
}

fn finalize_blake2(blake2: Blake2bVar) -> String {
    let mut digest = [0; 32];
    blake2
        .finalize_variable(&mut digest)
        .expect("blake2b-256 output buffer has the requested size");
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn unix_now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use velodex_http::upload::UploadError;

    use super::upload_error_reason;

    #[test]
    fn test_upload_error_reason_formats_metadata_field_and_fallback() {
        assert_eq!(
            upload_error_reason(&UploadError::MetadataFieldMismatch {
                field: "Project-URL",
                metadata: "Homepage, https://example.test".to_owned(),
                form: "Source, https://example.test/src".to_owned(),
            }),
            "metadata field Project-URL is \"Homepage, https://example.test\", expected \"Source, https://example.test/src\""
        );
        assert_eq!(upload_error_reason(&UploadError::NotFileUpload), "NotFileUpload");
    }
}
