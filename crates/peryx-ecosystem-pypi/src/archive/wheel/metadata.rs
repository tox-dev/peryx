//! The PEP 658 `METADATA` sidecar: the document pypi.org serves beside a wheel so a resolver reads a
//! distribution's metadata without downloading the wheel itself.

use std::io::{Cursor, Read, Seek};
use std::path::Path;

use super::{ArchiveError, MAX_WHEEL_METADATA_BYTES, expected_wheel_dist_info_dir, invalid_wheel, read_error};

/// Extract a wheel's `*.dist-info/METADATA` document, the file pypi.org serves as the PEP 658
/// sibling of an upload. Returns `None` for non-wheels or wheels without one.
#[must_use]
pub fn wheel_metadata(filename: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    wheel_metadata_reader(filename, Cursor::new(bytes)).ok().flatten()
}

/// The exact wheel metadata member implied by a wheel filename.
///
/// # Errors
/// Returns [`ArchiveError::InvalidWheel`] when `filename` ends with `.whl` but is not a valid
/// wheel filename.
pub fn wheel_metadata_member_path(filename: &str) -> Result<Option<String>, ArchiveError> {
    if !is_wheel(filename) {
        return Ok(None);
    }
    Ok(Some(format!("{}/METADATA", expected_wheel_dist_info_dir(filename)?)))
}

/// Extract a wheel's `*.dist-info/METADATA` document from a staged file without buffering the wheel.
///
/// # Errors
/// Returns [`ArchiveError::Read`] when the staged file or ZIP cannot be read.
pub fn wheel_metadata_path(filename: &str, path: &Path) -> Result<Option<Vec<u8>>, ArchiveError> {
    let file = std::fs::File::open(path).map_err(read_error)?;
    wheel_metadata_reader(filename, file)
}

fn wheel_metadata_reader(filename: &str, reader: impl Read + Seek) -> Result<Option<Vec<u8>>, ArchiveError> {
    let Some(metadata_path) = wheel_metadata_member_path(filename)? else {
        return Ok(None);
    };
    let mut archive = zip::ZipArchive::new(reader).map_err(read_error)?;
    let mut entry = match archive.by_name(&metadata_path) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(err) => return Err(read_error(err)),
    };
    if !entry.is_file() {
        return Ok(None);
    }
    if entry.size() > MAX_WHEEL_METADATA_BYTES {
        return Err(invalid_wheel(format!(
            "{metadata_path} is {} bytes, above the upload validation limit of {MAX_WHEEL_METADATA_BYTES} bytes",
            entry.size()
        )));
    }
    let mut bytes = Vec::with_capacity(entry.size().min(256 * 1024) as usize);
    entry.read_to_end(&mut bytes).map_err(read_error)?;
    Ok(Some(bytes))
}

fn is_wheel(filename: &str) -> bool {
    std::path::Path::new(filename)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
}
