//! The legacy `PyPI` multipart upload API, used unchanged by both twine and `uv publish`.
//!
//! The wire logic here is pure: authentication is a header check, and turning a parsed multipart
//! form into a stored file is validation plus content addressing. The async multipart reading lives
//! in the handler; everything it depends on is unit-testable without a server.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use velodex_core::pypi::{CoreMetadata, File, Yanked, normalize_name};
use velodex_storage::blob::Digest;

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
    pub sha256_digest: Option<String>,
    pub filename: Option<String>,
    pub content: Option<Vec<u8>>,
}

/// Why an upload was rejected, mapped to an HTTP status by the handler.
#[derive(Debug, PartialEq, Eq)]
pub enum UploadError {
    /// `:action` was not `file_upload`.
    NotFileUpload,
    /// A required field was missing.
    Missing(&'static str),
    /// The client's declared `sha256_digest` did not match the content.
    DigestMismatch,
    /// The uploaded filename is not a safe URL path segment.
    InvalidFilename(String),
}

/// A validated, content-addressed upload ready to be stored.
#[derive(Debug, PartialEq, Eq)]
pub struct PreparedUpload {
    pub normalized: String,
    pub display_name: String,
    pub filename: String,
    pub digest: Digest,
    pub content: Vec<u8>,
    pub record: Uploaded,
}

/// Validate a parsed upload form and turn it into a stored-file record addressed by the content's
/// sha256, with its download URL pointing at velodex's own file route on `index`.
///
/// # Errors
/// Returns [`UploadError`] if the action is wrong, a required field is missing, or a declared digest
/// does not match the content, or the filename is not a safe URL path segment.
pub fn prepare(form: UploadForm, index: &str) -> Result<PreparedUpload, UploadError> {
    if form.action.as_deref() != Some("file_upload") {
        return Err(UploadError::NotFileUpload);
    }
    let name = form.name.ok_or(UploadError::Missing("name"))?;
    let version = form.version.ok_or(UploadError::Missing("version"))?;
    let filename = form.filename.ok_or(UploadError::Missing("filename"))?;
    validate_filename(&filename).map_err(|_| UploadError::InvalidFilename(filename.clone()))?;
    let content = form.content.ok_or(UploadError::Missing("content"))?;
    let digest = Digest::of(&content);
    if let Some(declared) = form.sha256_digest.as_deref()
        && declared != digest.as_str()
    {
        return Err(UploadError::DigestMismatch);
    }
    let normalized = normalize_name(&name);
    let file = File {
        filename: filename.clone(),
        url: local_file_url(index, digest.as_str(), &filename),
        hashes: BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
        requires_python: form.requires_python,
        size: Some(content.len() as u64),
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
    };
    Ok(PreparedUpload {
        normalized,
        display_name: name,
        filename,
        digest,
        content,
        record: Uploaded { version, file },
    })
}
