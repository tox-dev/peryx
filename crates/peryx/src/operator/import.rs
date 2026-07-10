//! Importing local wheel and sdist files into a hosted index.

use std::fs::File;
use std::io::{BufReader, Read as _, Write};
use std::path::Path;

use anyhow::{Context as _, bail};
use blake2::Blake2bVar;
use blake2::digest::{Update as _, VariableOutput as _};
use peryx_ecosystem_pypi::upload::{self, StagedUpload, UploadError, UploadForm};
use peryx_ecosystem_pypi::{DistributionFilename, DistributionFilenameError, parse_distribution_filename};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::{BUFFER_BYTES, finalize_blake2, unix_now};
use crate::config::Config;

/// Import local wheel and sdist files into a hosted index.
///
/// # Errors
/// Returns an error if the data directory cannot be opened, the index cannot accept imported
/// files, or output fails.
pub fn import_dir(config: &Config, selector: &str, dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    if !dir.is_dir() {
        bail!("import directory {} does not exist", dir.display());
    }
    let target = import_target(config, selector)?;
    std::fs::create_dir_all(&config.data_dir)
        .context(format!("create data directory {}", config.data_dir.display()))?;
    let open_context = format!("open metadata store {}", config.data_dir.join("peryx.redb").display());
    let meta = MetaStore::open(config.data_dir.join("peryx.redb")).context(open_context)?;
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

fn import_target(config: &Config, selector: &str) -> anyhow::Result<ImportTarget> {
    let indexes = crate::server::build_indexes(&config.indexes, config.offline)?;
    let position = indexes
        .iter()
        .position(|index| index.name == selector)
        .or_else(|| indexes.iter().position(|index| index.route == selector))
        .context(format!("unknown index {selector:?}"))?;
    let index = &indexes[position];
    if index.ecosystem != peryx_core::Ecosystem::Pypi {
        bail!(
            "import-dir imports PyPI wheels and sdists; index {selector:?} serves the {} ecosystem",
            index.ecosystem
        );
    }
    match &index.kind {
        peryx_http::IndexKind::Hosted { .. } => Ok(ImportTarget {
            name: index.name.clone(),
            route: index.route.clone(),
        }),
        peryx_http::IndexKind::Virtual {
            upload: Some(upload), ..
        } => {
            let target = &indexes[*upload];
            Ok(ImportTarget {
                name: target.name.clone(),
                route: index.route.clone(),
            })
        }
        peryx_http::IndexKind::Virtual { upload: None, .. } => {
            bail!("index {selector:?} has no hosted upload target")
        }
        peryx_http::IndexKind::Cached { .. } => bail!("index {selector:?} is read-only"),
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

#[cfg(test)]
mod tests {
    use peryx_ecosystem_pypi::upload::UploadError;

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
