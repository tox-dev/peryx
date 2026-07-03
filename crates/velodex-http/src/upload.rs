//! The legacy `PyPI` multipart upload API, used unchanged by both twine and `uv publish`.
//!
//! The wire logic here is pure: authentication is a header check, and turning a parsed multipart
//! form into a stored file is validation plus content addressing. The async multipart reading lives
//! in the handler; everything it depends on is unit-testable without a server.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use velodex_core::pypi::{
    CoreMetadata, DistributionFilename, DistributionFilenameError, DistributionKind, File, Yanked, is_valid_name,
    normalize_name, parse_distribution_filename, parse_metadata, parse_version, parse_version_specifiers,
};
use velodex_storage::blob::{Digest, StagedBlob};

use crate::path_safety::{local_file_url, validate_filename};

/// An uploaded file plus the version it belongs to, stored per file on a private index and
/// reassembled into the project's detail page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uploaded {
    pub version: String,
    pub file: File,
}

/// Whether an `Authorization` header carries the correct upload token as its Basic-auth password.
/// Any username is accepted, matching pypi's `__token__` convention where the password is the token.
#[must_use]
pub fn authorized(header: Option<&str>, token: &str) -> bool {
    let Some(basic) = header.and_then(|value| value.strip_prefix("Basic ")) else {
        return false;
    };
    let Ok(decoded) = STANDARD.decode(basic.trim()) else {
        return false;
    };
    let Ok(credentials) = String::from_utf8(decoded) else {
        return false;
    };
    credentials
        .split_once(':')
        .is_some_and(|(_user, password)| password == token)
}

/// The fields velodex reads from an upload's multipart form. Every field is optional here so the
/// handler can collect whatever parts arrive; [`prepare`] enforces what is required.
#[derive(Debug, Default)]
pub struct UploadForm {
    pub action: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub requires_python: Option<String>,
    pub filetype: Option<String>,
    pub sha256_digest: Option<String>,
    pub blake2_256_digest: Option<String>,
    pub md5_digest: Option<String>,
    pub filename: Option<String>,
}

/// An upload body staged on disk while the multipart stream was read.
#[derive(Debug)]
pub struct StagedUpload {
    pub blob: StagedBlob,
    pub blake2_256: String,
}

/// Why an upload was rejected, mapped to an HTTP status by the handler.
#[derive(Debug, PartialEq, Eq)]
pub enum UploadError {
    /// `:action` was not `file_upload`.
    NotFileUpload,
    /// A required field was missing.
    Missing(&'static str),
    /// The form project name does not match the `PyPA` project-name grammar.
    InvalidName(String),
    /// The form version does not parse as PEP 440.
    InvalidVersion(String),
    /// The uploaded filename is not a safe URL path segment.
    InvalidFilename(String),
    /// The filename is not an accepted upload distribution format.
    InvalidDistributionFilename {
        filename: String,
        error: DistributionFilenameError,
    },
    /// `filetype` does not match the distribution filename.
    FiletypeMismatch { expected: String, actual: String },
    /// The filename project does not match the upload form.
    FilenameNameMismatch { filename: String, form: String },
    /// The filename version does not match the upload form.
    FilenameVersionMismatch { filename: String, form: String },
    /// The client's declared digest did not match the content.
    DigestMismatch(&'static str),
    /// The upload only supplied a legacy md5 digest.
    Md5Only,
    /// A declared digest was malformed.
    InvalidDigest { field: &'static str, value: String },
    /// `Requires-Python` was not a valid version specifier set.
    InvalidRequiresPython(String),
    /// The archive could not be read as the format its filename claims.
    InvalidContent(String),
    /// The artifact did not contain the metadata document needed to verify identity.
    MissingMetadata(&'static str),
    /// The metadata document was not UTF-8.
    InvalidMetadataUtf8,
    /// The metadata project name does not match the upload form.
    MetadataNameMismatch { metadata: String, form: String },
    /// The metadata version does not match the upload form.
    MetadataVersionMismatch { metadata: String, form: String },
    /// The upload time could not be represented as RFC 3339.
    InvalidUploadTime,
}

/// A validated, content-addressed upload ready to be stored.
#[derive(Debug)]
pub struct PreparedUpload {
    pub normalized: String,
    pub display_name: String,
    pub filename: String,
    pub digest: Digest,
    pub content: StagedBlob,
    pub metadata: Option<Vec<u8>>,
    pub record: Uploaded,
}

/// Validate a parsed upload form and turn it into a stored-file record addressed by the content's
/// sha256, with its download URL pointing at velodex's own file route on `index`.
///
/// # Errors
/// Returns [`UploadError`] if the action is wrong, a required field is missing, or a declared digest
/// does not match the content, or the filename is not a safe URL path segment.
pub fn prepare(
    form: UploadForm,
    staged: StagedUpload,
    index: &str,
    upload_time_unix: i64,
) -> Result<PreparedUpload, UploadError> {
    if form.action.as_deref() != Some("file_upload") {
        return Err(UploadError::NotFileUpload);
    }
    let name = form.name.ok_or(UploadError::Missing("name"))?;
    if !is_valid_name(&name) {
        return Err(UploadError::InvalidName(name));
    }
    let version = form.version.ok_or(UploadError::Missing("version"))?;
    let Some(parsed_version) = parse_version(&version) else {
        return Err(UploadError::InvalidVersion(version));
    };
    let filename = form.filename.ok_or(UploadError::Missing("filename"))?;
    validate_filename(&filename).map_err(|_| UploadError::InvalidFilename(filename.clone()))?;
    let normalized = normalize_name(&name);
    let parsed = parse_filename(&filename)?;
    if parsed.normalized_name != normalized {
        return Err(UploadError::FilenameNameMismatch {
            filename: parsed.name,
            form: name,
        });
    }
    if parsed.version != parsed_version {
        return Err(UploadError::FilenameVersionMismatch {
            filename: parsed.version.to_string(),
            form: version,
        });
    }
    let filetype = form.filetype.ok_or(UploadError::Missing("filetype"))?;
    if filetype != parsed.kind.upload_filetype() {
        return Err(UploadError::FiletypeMismatch {
            expected: parsed.kind.upload_filetype().to_owned(),
            actual: filetype,
        });
    }
    verify_declared_hashes(
        form.sha256_digest.as_deref(),
        form.blake2_256_digest.as_deref(),
        form.md5_digest.as_deref(),
        staged.blob.digest(),
        &staged.blake2_256,
    )?;
    let metadata = metadata_bytes(&parsed, &filename, staged.blob.path())?;
    let metadata_doc = std::str::from_utf8(&metadata).map_err(|_| UploadError::InvalidMetadataUtf8)?;
    let metadata_doc = parse_metadata(metadata_doc);
    if normalize_name(&metadata_doc.name) != normalized || !is_valid_name(&metadata_doc.name) {
        return Err(UploadError::MetadataNameMismatch {
            metadata: metadata_doc.name,
            form: normalized,
        });
    }
    let Some(metadata_version) = parse_version(&metadata_doc.version) else {
        return Err(UploadError::MetadataVersionMismatch {
            metadata: metadata_doc.version,
            form: parsed_version.to_string(),
        });
    };
    if metadata_version != parsed_version {
        return Err(UploadError::MetadataVersionMismatch {
            metadata: metadata_version.to_string(),
            form: parsed_version.to_string(),
        });
    }
    let requires_python = form
        .requires_python
        .filter(|requires_python| !requires_python.trim().is_empty())
        .or(metadata_doc.requires_python)
        .map(validate_requires_python)
        .transpose()?;
    let upload_time = upload_time(upload_time_unix)?;
    let digest = staged.blob.digest().clone();
    let file = File {
        filename: filename.clone(),
        url: local_file_url(index, digest.as_str(), &filename),
        hashes: BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
        requires_python,
        size: Some(staged.blob.len()),
        upload_time: Some(upload_time),
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
    };
    Ok(PreparedUpload {
        normalized,
        display_name: metadata_doc.name,
        filename,
        digest,
        content: staged.blob,
        metadata: (parsed.kind == DistributionKind::Wheel).then_some(metadata),
        record: Uploaded { version, file },
    })
}

fn parse_filename(filename: &str) -> Result<DistributionFilename, UploadError> {
    parse_distribution_filename(filename).map_err(|error| UploadError::InvalidDistributionFilename {
        filename: filename.to_owned(),
        error,
    })
}

fn verify_declared_hashes(
    sha256_digest: Option<&str>,
    blake2_256_digest: Option<&str>,
    md5_digest: Option<&str>,
    sha256: &Digest,
    blake2_256: &str,
) -> Result<(), UploadError> {
    let has_sha256 = verify_declared_hash("sha256_digest", sha256_digest, sha256.as_str())?;
    let has_blake2 = verify_declared_hash("blake2_256_digest", blake2_256_digest, blake2_256)?;
    if !has_sha256 && !has_blake2 && md5_digest.is_some_and(|digest| !digest.is_empty()) {
        return Err(UploadError::Md5Only);
    }
    Ok(())
}

fn verify_declared_hash(field: &'static str, declared: Option<&str>, actual: &str) -> Result<bool, UploadError> {
    let Some(declared) = declared.filter(|declared| !declared.is_empty()) else {
        return Ok(false);
    };
    if !is_lower_hex(declared) || declared.len() != actual.len() {
        return Err(UploadError::InvalidDigest {
            field,
            value: declared.to_owned(),
        });
    }
    if !constant_time_eq(declared.as_bytes(), actual.as_bytes()) {
        return Err(UploadError::DigestMismatch(field));
    }
    Ok(true)
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        diff |=
            usize::from(left.get(index).copied().unwrap_or_default() ^ right.get(index).copied().unwrap_or_default());
    }
    diff == 0
}

fn metadata_bytes(
    parsed: &DistributionFilename,
    filename: &str,
    path: &std::path::Path,
) -> Result<Vec<u8>, UploadError> {
    match parsed.kind {
        DistributionKind::Wheel => crate::archive::validate_wheel_path(filename, path)
            .map_err(|err| UploadError::InvalidContent(err.to_string())),
        DistributionKind::SdistTarGz => crate::archive::sdist_metadata_path(filename, path)
            .map_err(|err| UploadError::InvalidContent(err.to_string()))?
            .ok_or(UploadError::MissingMetadata("PKG-INFO")),
    }
}

fn validate_requires_python(value: String) -> Result<String, UploadError> {
    if parse_version_specifiers(&value).is_some() {
        Ok(value)
    } else {
        Err(UploadError::InvalidRequiresPython(value))
    }
}

fn upload_time(timestamp: i64) -> Result<String, UploadError> {
    time::OffsetDateTime::from_unix_timestamp(timestamp)
        .map_err(|_| UploadError::InvalidUploadTime)?
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| UploadError::InvalidUploadTime)
}
