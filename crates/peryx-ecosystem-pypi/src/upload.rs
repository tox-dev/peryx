//! The legacy `PyPI` multipart upload API, used unchanged by both twine and `uv publish`.
//!
//! The wire logic here is pure: authentication is a header check, and turning a parsed multipart
//! form into a stored file is validation plus content addressing. The async multipart reading lives
//! in the handler; everything it depends on is unit-testable without a server.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};

use md5::{Digest as _, Md5};
use url::Url;

use crate::archive::ValidatedArchive;
use crate::{
    CoreMetadata, CoreMetadataDoc, DistributionFilename, DistributionFilenameError, DistributionKind, File,
    MetadataError, Provenance, Yanked, is_valid_name, normalize_name, normalize_name_cow, parse_distribution_filename,
    parse_metadata, parse_version, parse_version_specifiers, to_json,
};
use peryx_storage::blob::{BlobError, BlobStore, Digest, StagedBlob};
use peryx_storage::meta::{MetaError, MetaStore};

use crate::store::PypiStore as _;
use crate::store::{Guard, MetadataSibling, PublishedFile};
use serde::{Deserialize, Serialize};

use peryx_core::path::{local_file_url, validate_filename};

/// An uploaded file plus the version it belongs to, stored per file on a private index and
/// reassembled into the project's detail page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uploaded {
    pub version: String,
    pub file: File,
}

/// The fields peryx reads from an upload's multipart form. Every field is optional here so the
/// handler can collect whatever parts arrive; [`prepare`] enforces what is required.
#[derive(Debug, Default)]
pub struct UploadForm {
    pub action: Option<String>,
    pub metadata_version: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub requires_python: Option<String>,
    pub license: Option<String>,
    pub license_expression: Option<String>,
    pub license_files: Vec<String>,
    pub provides_extra: Vec<String>,
    pub project_urls: Vec<String>,
    pub home_page: Option<String>,
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
    /// A declared digest was malformed.
    InvalidDigest { field: &'static str, value: String },
    /// `Requires-Python` was not a valid version specifier set.
    InvalidRequiresPython(String),
    /// The archive could not be read as the format its filename claims.
    InvalidContent(String),
    /// The metadata document was not UTF-8.
    InvalidMetadataUtf8,
    /// The metadata document's header block is not a well-formed RFC 822 message.
    MalformedMetadata(MetadataError),
    /// `Project-URL` did not contain a 1-32 character label and an HTTP(S) URL.
    InvalidProjectUrl { label: String, url: String },
    /// `License-File` did not locate a file below the project root.
    InvalidLicenseFile { value: String, reason: &'static str },
    /// The metadata document contained both `License` and `License-Expression`.
    ConflictingLicenseFields,
    /// Core Metadata semantics depend on a declared version.
    MissingMetadataVersion,
    /// Peryx cannot apply semantics from an unsupported Core Metadata version.
    UnsupportedMetadataVersion(String),
    /// Core Metadata assigns version-specific constraints to field values.
    InvalidMetadataValue {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
    /// The metadata project name does not match the upload form.
    MetadataNameMismatch { metadata: String, form: String },
    /// The metadata version does not match the upload form.
    MetadataVersionMismatch { metadata: String, form: String },
    /// A metadata field does not match the upload form.
    MetadataFieldMismatch {
        field: &'static str,
        metadata: String,
        form: String,
    },
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
    pub metadata: Vec<u8>,
    pub record: Uploaded,
}

/// An error while committing a validated upload to storage.
#[derive(Debug, thiserror::Error)]
pub enum UploadStoreError {
    #[error(transparent)]
    Meta(#[from] MetaError),
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error(transparent)]
    Parse(#[from] serde_json::Error),
    #[error("file already exists with different content: {0}")]
    FileExists(String),
}

/// Validate a parsed upload form and turn it into a stored-file record addressed by the content's
/// sha256, with its download URL pointing at peryx's own file route on `index`.
///
/// # Errors
/// Returns [`UploadError`] if the action is wrong, a required field is missing, or a declared digest
/// does not match the content, or the filename is not a safe URL path segment.
pub fn prepare(
    mut form: UploadForm,
    staged: StagedUpload,
    index: &str,
    upload_time_unix: i64,
) -> Result<PreparedUpload, UploadError> {
    if form.action.as_deref() != Some("file_upload") {
        return Err(UploadError::NotFileUpload);
    }
    let name = form.name.take().ok_or(UploadError::Missing("name"))?;
    if !is_valid_name(&name) {
        return Err(UploadError::InvalidName(name));
    }
    let version = form.version.take().ok_or(UploadError::Missing("version"))?;
    let Some(parsed_version) = parse_version(&version) else {
        return Err(UploadError::InvalidVersion(version));
    };
    let filename = form.filename.take().ok_or(UploadError::Missing("filename"))?;
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
    let filetype = form.filetype.take().ok_or(UploadError::Missing("filetype"))?;
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
        staged.blob.path(),
    )?;
    let ValidatedArchive {
        metadata,
        missing_license_files,
    } = validate_archive(&parsed, &filename, staged.blob.path())?;
    let metadata_text = std::str::from_utf8(&metadata).map_err(|_| UploadError::InvalidMetadataUtf8)?;
    let metadata_doc = parse_metadata(metadata_text).map_err(UploadError::MalformedMetadata)?;
    let form_requires_python = form
        .requires_python
        .clone()
        .filter(|requires_python| !requires_python.trim().is_empty())
        .map(validate_requires_python)
        .transpose()?;
    validate_metadata_identity(&form, &metadata_doc, &normalized, &parsed_version)?;
    if let Some(value) = missing_license_files.into_iter().next() {
        return Err(UploadError::InvalidLicenseFile {
            value,
            reason: "the archive does not carry the declared file",
        });
    }
    let requires_python = metadata_doc
        .requires_python
        .map(validate_requires_python)
        .transpose()?
        .or(form_requires_python);
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
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::Absent,
    };
    Ok(PreparedUpload {
        normalized,
        display_name: metadata_doc.name,
        filename,
        digest,
        content: staged.blob,
        metadata,
        record: Uploaded { version, file },
    })
}

/// Persist a validated upload into a local store. Returns `false` when the same file and digest are
/// already present.
///
/// # Errors
/// Returns [`UploadStoreError`] if a blob write, metadata write, or existing-record decode fails.
pub fn store_prepared(
    meta: &MetaStore,
    blobs: &BlobStore,
    name: &str,
    prepared: PreparedUpload,
) -> Result<bool, UploadStoreError> {
    let PreparedUpload {
        normalized,
        display_name,
        filename,
        digest: content_digest,
        content,
        metadata,
        mut record,
    } = prepared;
    blobs.commit_staged(content)?;
    let metadata_digest = blobs.write(&metadata)?;
    let hashes = BTreeMap::from([("sha256".to_owned(), metadata_digest.as_str().to_owned())]);
    record.file.set_metadata(CoreMetadata::Hashes(hashes));
    let body = to_json(&record).into_bytes();
    meta.publish_file_if(
        &PublishedFile {
            index: name,
            normalized: &normalized,
            display: &display_name,
            filename: &filename,
            record: &body,
            version: record.version.as_str(),
            metadata: Some(MetadataSibling {
                artifact_sha256: content_digest.as_str(),
                url: "uploaded",
                metadata_sha256: metadata_digest.as_str(),
                source: name,
            }),
        },
        |existing| upload_conflict(existing, content_digest.as_str(), &filename),
    )
}

/// The upload publish precondition, evaluated inside the write transaction: a first upload commits,
/// an identical re-upload is an idempotent no-op, and the same filename with different bytes is a
/// conflict — so two concurrent different-content uploads cannot both publish.
fn upload_conflict(existing: Option<&[u8]>, digest: &str, filename: &str) -> Result<Guard, UploadStoreError> {
    let Some(existing) = existing else {
        return Ok(Guard::Commit);
    };
    let uploaded: Uploaded = serde_json::from_slice(existing)?;
    if uploaded.file.hashes.get("sha256").is_some_and(|hash| hash == digest) {
        Ok(Guard::Skip)
    } else {
        Err(UploadStoreError::FileExists(filename.to_owned()))
    }
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
    content_path: &std::path::Path,
) -> Result<(), UploadError> {
    let has_sha256 = verify_declared_hash("sha256_digest", sha256_digest, sha256.as_str())?;
    let has_blake2 = verify_declared_hash("blake2_256_digest", blake2_256_digest, blake2_256)?;
    // Legacy tooling sends md5_digest alone; peryx verifies it like Warehouse rather than rejecting.
    // Skip the extra content read when a stronger declared digest already covers the bytes.
    if let Some(md5_digest) = md5_digest.filter(|digest| !has_sha256 && !has_blake2 && !digest.is_empty()) {
        verify_declared_hash("md5_digest", Some(md5_digest), &content_md5(content_path)?)?;
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

fn content_md5(path: &std::path::Path) -> Result<String, UploadError> {
    let invalid_content = |err: std::io::Error| UploadError::InvalidContent(err.to_string());
    let mut content =
        std::io::BufReader::with_capacity(64 * 1024, std::fs::File::open(path).map_err(&invalid_content)?);
    let mut md5 = Md5::new();
    std::io::copy(&mut content, &mut md5).map_err(&invalid_content)?;
    Ok(to_hex(md5.finalize().as_slice()))
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
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

fn validate_archive(
    parsed: &DistributionFilename,
    filename: &str,
    path: &std::path::Path,
) -> Result<ValidatedArchive, UploadError> {
    let validate = match parsed.kind {
        DistributionKind::Wheel => crate::archive::validate_wheel_path,
        DistributionKind::SdistTarGz => crate::archive::validate_sdist_path,
        DistributionKind::SdistZip => crate::archive::validate_zip_sdist_path,
    };
    validate(filename, path).map_err(|err| UploadError::InvalidContent(err.to_string()))
}

fn validate_metadata_identity(
    form: &UploadForm,
    metadata: &crate::CoreMetadataDoc,
    normalized: &str,
    parsed_version: &crate::Version,
) -> Result<(), UploadError> {
    let declared = validate_metadata_version(metadata.metadata_version.as_deref())?;
    validate_field_introductions(metadata, declared)?;
    if normalize_name(&metadata.name) != normalized || !is_valid_name(&metadata.name) {
        return Err(UploadError::MetadataNameMismatch {
            metadata: metadata.name.clone(),
            form: normalized.to_owned(),
        });
    }
    let Some(metadata_version) = parse_version(&metadata.version) else {
        return Err(UploadError::MetadataVersionMismatch {
            metadata: metadata.version.clone(),
            form: parsed_version.to_string(),
        });
    };
    if &metadata_version != parsed_version {
        return Err(UploadError::MetadataVersionMismatch {
            metadata: metadata_version.to_string(),
            form: parsed_version.to_string(),
        });
    }
    if metadata.license.is_some() && metadata.license_expression.is_some() {
        return Err(UploadError::ConflictingLicenseFields);
    }
    validate_license_expression(metadata)?;
    validate_provided_extras(metadata)?;
    validate_classifiers(metadata)?;
    validate_requirements(metadata)?;
    compare_metadata_field(
        "Metadata-Version",
        form.metadata_version.as_deref(),
        metadata.metadata_version.as_deref(),
    )?;
    compare_metadata_field(
        "Requires-Python",
        form.requires_python.as_deref(),
        metadata.requires_python.as_deref(),
    )?;
    compare_metadata_field("License", form.license.as_deref(), metadata.license.as_deref())?;
    compare_metadata_field(
        "License-Expression",
        form.license_expression.as_deref(),
        metadata.license_expression.as_deref(),
    )?;
    validate_license_files(&metadata.license_files)?;
    compare_metadata_list("License-File", &form.license_files, &metadata.license_files)?;
    compare_metadata_list("Provides-Extra", &form.provides_extra, &metadata.provides_extra)?;
    compare_project_urls(form, metadata)
}

/// A supported Core Metadata version, ranked as `major * 10 + minor` so a declared version orders
/// against the version that introduced a field.
fn validate_metadata_version(value: Option<&str>) -> Result<u8, UploadError> {
    let Some(value) = value else {
        return Err(UploadError::MissingMetadataVersion);
    };
    match value {
        "1.0" => Ok(10),
        "1.1" => Ok(11),
        "1.2" => Ok(12),
        "2.1" => Ok(21),
        "2.2" => Ok(22),
        "2.3" => Ok(23),
        "2.4" => Ok(24),
        "2.5" => Ok(25),
        "2.6" => Ok(26),
        _ => Err(UploadError::UnsupportedMetadataVersion(value.to_owned())),
    }
}

/// Core Metadata introduced each field in a version, and a document may not use a field that
/// postdates the version it declares. Deprecating a field leaves it usable — `License` still reads
/// under 2.4 — so an introduction is the only bound.
///
/// Versions follow the field history in
/// <https://packaging.python.org/en/latest/specifications/core-metadata/>.
fn validate_field_introductions(metadata: &CoreMetadataDoc, declared: u8) -> Result<(), UploadError> {
    const SINCE_1_1: (u8, &str) = (11, "requires Metadata-Version 1.1 or later");
    const SINCE_1_2: (u8, &str) = (12, "requires Metadata-Version 1.2 or later");
    const SINCE_2_1: (u8, &str) = (21, "requires Metadata-Version 2.1 or later");
    const SINCE_2_4: (u8, &str) = (24, "requires Metadata-Version 2.4 or later");

    for (field, (since, reason), value) in [
        ("Classifier", SINCE_1_1, metadata.classifiers.first().map(Cow::from)),
        ("Maintainer", SINCE_1_2, metadata.maintainer.as_ref().map(Cow::from)),
        (
            "Requires-Python",
            SINCE_1_2,
            metadata.requires_python.as_ref().map(Cow::from),
        ),
        (
            "Requires-Dist",
            SINCE_1_2,
            metadata.requires_dist.first().map(Cow::from),
        ),
        (
            "Provides-Dist",
            SINCE_1_2,
            metadata.provides_dist.first().map(Cow::from),
        ),
        (
            "Obsoletes-Dist",
            SINCE_1_2,
            metadata.obsoletes_dist.first().map(Cow::from),
        ),
        (
            "Project-URL",
            SINCE_1_2,
            metadata
                .project_urls
                .first()
                .map(|(label, url)| Cow::Owned(format!("{label}, {url}"))),
        ),
        (
            "Description-Content-Type",
            SINCE_2_1,
            metadata.description_content_type.as_ref().map(Cow::from),
        ),
        (
            "Provides-Extra",
            SINCE_2_1,
            metadata.provides_extra.first().map(Cow::from),
        ),
        (
            "License-Expression",
            SINCE_2_4,
            metadata.license_expression.as_ref().map(Cow::from),
        ),
        ("License-File", SINCE_2_4, metadata.license_files.first().map(Cow::from)),
    ] {
        if let Some(value) = value
            && declared < since
        {
            return Err(UploadError::InvalidMetadataValue {
                field,
                value: value.into_owned(),
                reason,
            });
        }
    }
    Ok(())
}

fn validate_license_expression(metadata: &CoreMetadataDoc) -> Result<(), UploadError> {
    let Some(expression) = metadata.license_expression.as_deref() else {
        return Ok(());
    };
    crate::license::validate_expression(expression).map_err(|reason| UploadError::InvalidMetadataValue {
        field: "License-Expression",
        value: expression.to_owned(),
        reason,
    })
}

fn validate_provided_extras(metadata: &CoreMetadataDoc) -> Result<(), UploadError> {
    let normalized_required = matches!(
        metadata.metadata_version.as_deref(),
        Some("2.3" | "2.4" | "2.5" | "2.6")
    );
    let mut seen = HashSet::with_capacity(metadata.provides_extra.len());
    for value in &metadata.provides_extra {
        if !is_valid_name(value) {
            return Err(UploadError::InvalidMetadataValue {
                field: "Provides-Extra",
                value: value.clone(),
                reason: if normalized_required {
                    "must match ^[a-z0-9]+(-[a-z0-9]+)*$"
                } else {
                    "must be a valid project or extra name"
                },
            });
        }
        let normalized = normalize_name_cow(value);
        if normalized_required && normalized.as_ref() != value {
            return Err(UploadError::InvalidMetadataValue {
                field: "Provides-Extra",
                value: value.clone(),
                reason: "must match ^[a-z0-9]+(-[a-z0-9]+)*$",
            });
        }
        if !seen.insert(normalized) {
            return Err(UploadError::InvalidMetadataValue {
                field: "Provides-Extra",
                value: value.clone(),
                reason: "duplicates an earlier value after normalization",
            });
        }
    }
    Ok(())
}

fn validate_classifiers(metadata: &CoreMetadataDoc) -> Result<(), UploadError> {
    for value in &metadata.classifiers {
        crate::classifier::validate(value).map_err(|reason| UploadError::InvalidMetadataValue {
            field: "Classifier",
            value: value.clone(),
            reason,
        })?;
    }
    Ok(())
}

/// `Requires-Dist`, `Provides-Dist`, and `Obsoletes-Dist` share the PEP 508 requirement grammar, so
/// one parser checks all three and the first malformed value reports its own field. Validation reads
/// the grammar only; the declared text stays verbatim for display.
fn validate_requirements(metadata: &CoreMetadataDoc) -> Result<(), UploadError> {
    for (field, values) in [
        ("Requires-Dist", &metadata.requires_dist),
        ("Provides-Dist", &metadata.provides_dist),
        ("Obsoletes-Dist", &metadata.obsoletes_dist),
    ] {
        for value in values {
            crate::requirement::validate(value).map_err(|reason| UploadError::InvalidMetadataValue {
                field,
                value: value.clone(),
                reason,
            })?;
        }
    }
    Ok(())
}

fn compare_metadata_field(field: &'static str, form: Option<&str>, metadata: Option<&str>) -> Result<(), UploadError> {
    let Some(form) = form.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    if metadata == Some(form) {
        Ok(())
    } else {
        Err(UploadError::MetadataFieldMismatch {
            field,
            metadata: metadata.unwrap_or_default().to_owned(),
            form: form.to_owned(),
        })
    }
}

/// Core Metadata locates each `License-File` below the project root, as `packaging` does: no parent
/// components, no unresolved globs, relative, and `/`-delimited.
fn validate_license_files(values: &[String]) -> Result<(), UploadError> {
    for value in values {
        let reason = if value.contains("..") {
            "parent directory components are not allowed"
        } else if value.contains('*') {
            "paths must be resolved"
        } else if is_absolute_path(value) {
            "paths must be relative"
        } else if value.contains('\\') {
            "paths must use the '/' delimiter"
        } else {
            continue;
        };
        return Err(UploadError::InvalidLicenseFile {
            value: value.clone(),
            reason,
        });
    }
    Ok(())
}

/// A Windows drive root (`C:/LICENSE`) is absolute without a leading separator.
fn is_absolute_path(value: &str) -> bool {
    value.starts_with('/') || matches!(value.as_bytes(), [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic())
}

fn compare_metadata_list(field: &'static str, form: &[String], metadata: &[String]) -> Result<(), UploadError> {
    let form = sorted_non_empty(form);
    if form.is_empty() {
        return Ok(());
    }
    let metadata = sorted_non_empty(metadata);
    if metadata == form {
        Ok(())
    } else {
        Err(UploadError::MetadataFieldMismatch {
            field,
            metadata: metadata.join(", "),
            form: form.join(", "),
        })
    }
}

fn compare_project_urls(form: &UploadForm, metadata: &CoreMetadataDoc) -> Result<(), UploadError> {
    if let Some((label, url)) = metadata.project_urls.iter().find(|(label, url)| {
        label.is_empty()
            || label.chars().count() > 32
            || !Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
    }) {
        return Err(UploadError::InvalidProjectUrl {
            label: label.clone(),
            url: url.clone(),
        });
    }
    let form_urls = upload_project_urls(form);
    if form_urls.is_empty() {
        return Ok(());
    }
    let mut metadata_urls = metadata.project_urls.clone();
    if let Some(home_page) = &metadata.home_page {
        metadata_urls.push(("Homepage".to_owned(), home_page.clone()));
    }
    metadata_urls.sort();
    if metadata_urls == form_urls {
        Ok(())
    } else {
        Err(UploadError::MetadataFieldMismatch {
            field: "Project-URL",
            metadata: metadata_urls
                .into_iter()
                .map(|(label, url)| format!("{label}, {url}"))
                .collect::<Vec<_>>()
                .join("; "),
            form: form_urls
                .into_iter()
                .map(|(label, url)| format!("{label}, {url}"))
                .collect::<Vec<_>>()
                .join("; "),
        })
    }
}

fn upload_project_urls(form: &UploadForm) -> Vec<(String, String)> {
    let mut urls: Vec<_> = form
        .project_urls
        .iter()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            let (label, url) = value.split_once(',').unwrap_or(("", value));
            (label.trim().to_owned(), url.trim().to_owned())
        })
        .collect();
    if let Some(home_page) = form.home_page.as_deref().filter(|value| !value.trim().is_empty()) {
        urls.push(("Homepage".to_owned(), home_page.to_owned()));
    }
    urls.sort();
    urls
}

fn sorted_non_empty(values: &[String]) -> Vec<String> {
    let mut values: Vec<_> = values
        .iter()
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .collect();
    values.sort();
    values
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
