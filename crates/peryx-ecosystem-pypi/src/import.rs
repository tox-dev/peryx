//! Importing local wheel and sdist files into a hosted `PyPI` index, the ecosystem half of
//! `peryx import-dir`. The neutral binary resolves the upload target from the index topology; this
//! walks the directory, validates each distribution, and stores it through the upload pipeline.

use std::fs::File;
use std::io::{BufReader, Read as _, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use blake2::Blake2bVar;
use blake2::digest::{Update as _, VariableOutput as _};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use crate::upload::{self, StagedUpload, UploadError, UploadForm};
use crate::{
    DistributionFilename, DistributionFilenameError, DistributionKind, Version, normalize_name,
    parse_distribution_filename, parse_metadata, parse_version,
};

const BUFFER_BYTES: usize = 1024 * 1024;

/// Import every wheel and sdist under `dir` into the hosted index `target_name` (reached at
/// `target_route`), writing one tab-separated line per file and a summary to `out`.
///
/// # Errors
/// Returns a message when the directory or a staged file cannot be read.
pub fn import_dir(
    meta: &MetaStore,
    blobs: &BlobStore,
    target_name: &str,
    target_route: &str,
    dir: &Path,
    out: &mut dyn Write,
) -> Result<(), String> {
    let target = Target {
        name: target_name,
        route: target_route,
    };
    let mut counts = ImportCounts::default();
    writeln!(out, "status\tfilename\tproject\tversion\treason").map_err(crate::error_message)?;
    walk_files(dir, &mut |path| {
        import_file(dir, path, target, meta, blobs, &mut counts, out)
    })
    .map_err(crate::error_message)?;
    let ImportCounts {
        imported,
        skipped,
        rejected,
    } = counts;
    writeln!(
        out,
        "summary\t\t\t\timported={imported} skipped={skipped} rejected={rejected}"
    )
    .map_err(crate::error_message)
}

#[derive(Default)]
struct ImportCounts {
    imported: u64,
    skipped: u64,
    rejected: u64,
}

/// The hosted index an import stores into: its own name, and the route uploads are prepared against.
#[derive(Clone, Copy)]
struct Target<'a> {
    name: &'a str,
    route: &'a str,
}

fn walk_files(dir: &Path, visit: &mut impl FnMut(&Path) -> std::io::Result<()>) -> std::io::Result<()> {
    let mut entries = std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_files(&entry.path(), visit)?;
        } else if file_type.is_file() {
            visit(&entry.path())?;
        }
    }
    Ok(())
}

fn import_file(
    root: &Path,
    path: &Path,
    target: Target<'_>,
    meta: &MetaStore,
    blobs: &BlobStore,
    counts: &mut ImportCounts,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let display = path.strip_prefix(root).unwrap_or(path).display().to_string();
    let filename = path
        .file_name()
        .expect("directory walk only visits named entries")
        .to_string_lossy();
    let parsed = match parse_distribution_filename(&filename) {
        Ok(parsed) => parsed,
        Err(DistributionFilenameError::UnsupportedExtension | DistributionFilenameError::LegacyEgg) => {
            counts.skipped += 1;
            return writeln!(out, "skipped\t{display}\t\t\tunsupported file type");
        }
        Err(err) => {
            counts.rejected += 1;
            return writeln!(out, "rejected\t{display}\t\t\tinvalid distribution filename: {err:?}");
        }
    };
    let staged = stage_file(path, blobs)?;
    if let Some((normalized, version)) = sdist_pkg_info_identity(&filename, &parsed, staged.blob.path())
        && (normalized != parsed.normalized_name || version != parsed.version)
    {
        counts.rejected += 1;
        let version = version.to_string();
        return writeln!(
            out,
            "rejected\t{display}\t{normalized}\t{version}\tsdist filename splits to a different project or version than its PKG-INFO"
        );
    }
    let version = parsed.version.to_string();
    let normalized = &parsed.normalized_name;
    match upload::prepare(
        upload_form(&filename, &parsed, &staged),
        staged,
        target.route,
        unix_now(),
    ) {
        Ok(prepared) => match upload::store_prepared(meta, blobs, target.name, prepared) {
            Ok(true) => {
                counts.imported += 1;
                writeln!(out, "imported\t{display}\t{normalized}\t{version}\tstored")
            }
            Ok(false) => {
                counts.skipped += 1;
                writeln!(out, "skipped\t{display}\t{normalized}\t{version}\talready present")
            }
            Err(err) => {
                counts.rejected += 1;
                writeln!(out, "rejected\t{display}\t{normalized}\t{version}\t{err}")
            }
        },
        Err(err) => {
            counts.rejected += 1;
            writeln!(
                out,
                "rejected\t{display}\t{normalized}\t{version}\t{}",
                upload_error_reason(&err)
            )
        }
    }
}

// Reconcile a legacy sdist's last-dash name/version split against its authoritative PKG-INFO. A
// non-sdist, an invalid archive, or unreadable metadata yields `None` so `prepare` reports the fault.
fn sdist_pkg_info_identity(filename: &str, parsed: &DistributionFilename, path: &Path) -> Option<(String, Version)> {
    let metadata = match parsed.kind {
        DistributionKind::SdistTarGz => crate::archive::validate_sdist_path(filename, path).ok()?,
        DistributionKind::SdistZip => crate::archive::validate_zip_sdist_path(filename, path).ok()?,
        DistributionKind::Wheel => return None,
    };
    let doc = parse_metadata(std::str::from_utf8(&metadata).ok()?);
    Some((normalize_name(&doc.name), parse_version(&doc.version)?))
}

fn stage_file(path: &Path, blobs: &BlobStore) -> std::io::Result<StagedUpload> {
    let mut input = BufReader::with_capacity(BUFFER_BYTES, File::open(path)?);
    let mut pending = blobs.begin().map_err(std::io::Error::other)?;
    let mut blake2 = Blake2bVar::new(32).expect("blake2b-256 output size is valid");
    let mut buffer = vec![0; BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        blake2.update(chunk);
        pending.write(chunk).map_err(std::io::Error::other)?;
    }
    Ok(StagedUpload {
        blob: pending.finish().map_err(std::io::Error::other)?,
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
        UploadError::ConflictingLicenseFields => "metadata contains both License and License-Expression".to_owned(),
        UploadError::MissingMetadataVersion => "metadata is missing Metadata-Version".to_owned(),
        UploadError::UnsupportedMetadataVersion(value) => format!("invalid Metadata-Version: {value:?}"),
        UploadError::InvalidMetadataValue { field, value, reason } => {
            format!("metadata {field} value {value:?} {reason}")
        }
        UploadError::InvalidRequiresPython(value) => format!("invalid Requires-Python: {value}"),
        UploadError::InvalidLicenseFile { value, reason } => format!("invalid License-File {value:?}: {reason}"),
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

fn finalize_blake2(blake2: Blake2bVar) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digest = [0; 32];
    blake2
        .finalize_variable(&mut digest)
        .expect("blake2b-256 output buffer has the requested size");
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
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
    use super::upload_error_reason;
    use crate::upload::UploadError;

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
