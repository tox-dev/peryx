//! Multipart upload parsing: drain fields, stage the content blob, and map upload errors to responses.
#![allow(
    clippy::result_large_err,
    reason = "handler helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use axum::extract::Multipart;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use blake2::Blake2bVar;
use blake2::digest::{Update as _, VariableOutput as _};

use crate::DistributionFilenameError;
use crate::upload::{StagedUpload, UploadError, UploadForm};

const MAX_UPLOAD_TEXT_FIELD_BYTES: usize = 64 * 1024;

/// Drain a multipart body into an [`UploadForm`], staging the `content` part on disk while the rest
/// stays as UTF-8 text. Unknown fields are ignored, as the upload API carries many metadata fields
/// peryx does not need. Every read or decode error funnels through [`reject`] as a 400.
pub(super) async fn collect_form(
    mut multipart: Multipart,
    blobs: &peryx_storage::blob::BlobStore,
) -> Result<(UploadForm, Option<StagedUpload>), Response> {
    let mut form = UploadForm::default();
    let mut staged = None;
    while let Some(field) = multipart.next_field().await.map_err(reject)? {
        let field_name = field.name().unwrap_or_default().to_owned();
        if field_name == "content" {
            if staged.is_some() {
                return Err(reject("duplicate content field"));
            }
            form.filename = field.file_name().map(str::to_owned);
            staged = Some(stage_content(field, blobs).await?);
        } else if let Some(upload_field) = upload_text_field(&field_name) {
            let value = read_text_field(field, &field_name).await?;
            set_upload_text_field(&mut form, upload_field, value);
        } else {
            drain_field(field).await?;
        }
    }
    Ok((form, staged))
}

#[derive(Clone, Copy)]
enum UploadTextField {
    Action,
    MetadataVersion,
    Name,
    Version,
    RequiresPython,
    License,
    LicenseExpression,
    LicenseFile,
    ProvidesExtra,
    ProjectUrl,
    HomePage,
    Filetype,
    Sha256Digest,
    Blake2Digest,
    Md5Digest,
}

fn upload_text_field(name: &str) -> Option<UploadTextField> {
    match name {
        ":action" => Some(UploadTextField::Action),
        "metadata_version" => Some(UploadTextField::MetadataVersion),
        "name" => Some(UploadTextField::Name),
        "version" => Some(UploadTextField::Version),
        "requires_python" => Some(UploadTextField::RequiresPython),
        "license" => Some(UploadTextField::License),
        "license_expression" => Some(UploadTextField::LicenseExpression),
        "license_file" | "license_files" => Some(UploadTextField::LicenseFile),
        "provides_extra" | "provides_extras" => Some(UploadTextField::ProvidesExtra),
        "project_urls" => Some(UploadTextField::ProjectUrl),
        "home_page" => Some(UploadTextField::HomePage),
        "filetype" => Some(UploadTextField::Filetype),
        "sha256_digest" => Some(UploadTextField::Sha256Digest),
        "blake2_256_digest" => Some(UploadTextField::Blake2Digest),
        "md5_digest" => Some(UploadTextField::Md5Digest),
        _ => None,
    }
}

fn set_upload_text_field(form: &mut UploadForm, field: UploadTextField, value: String) {
    match field {
        UploadTextField::Action => form.action = Some(value),
        UploadTextField::MetadataVersion => form.metadata_version = Some(value),
        UploadTextField::Name => form.name = Some(value),
        UploadTextField::Version => form.version = Some(value),
        UploadTextField::RequiresPython => form.requires_python = Some(value),
        UploadTextField::License => form.license = Some(value),
        UploadTextField::LicenseExpression => form.license_expression = Some(value),
        UploadTextField::LicenseFile => form.license_files.push(value),
        UploadTextField::ProvidesExtra => form.provides_extra.push(value),
        UploadTextField::ProjectUrl => form.project_urls.push(value),
        UploadTextField::HomePage => form.home_page = Some(value),
        UploadTextField::Filetype => form.filetype = Some(value),
        UploadTextField::Sha256Digest => form.sha256_digest = Some(value),
        UploadTextField::Blake2Digest => form.blake2_256_digest = Some(value),
        UploadTextField::Md5Digest => form.md5_digest = Some(value),
    }
}

async fn read_text_field(mut field: axum::extract::multipart::Field<'_>, name: &str) -> Result<String, Response> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(reject)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_UPLOAD_TEXT_FIELD_BYTES {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("upload field {name:?} exceeds {MAX_UPLOAD_TEXT_FIELD_BYTES} bytes"),
            )
                .into_response());
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(reject)
}

async fn drain_field(mut field: axum::extract::multipart::Field<'_>) -> Result<(), Response> {
    while field.chunk().await.map_err(reject)?.is_some() {}
    Ok(())
}

async fn stage_content(
    mut field: axum::extract::multipart::Field<'_>,
    blobs: &peryx_storage::blob::BlobStore,
) -> Result<StagedUpload, Response> {
    let mut pending = blobs.begin().map_err(storage_reject)?;
    let mut blake2 = Blake2bVar::new(32).expect("blake2b-256 output size is valid");
    while let Some(chunk) = field.chunk().await.map_err(reject)? {
        blake2.update(&chunk);
        pending.write(&chunk).map_err(storage_reject)?;
    }
    let mut digest = [0; 32];
    blake2
        .finalize_variable(&mut digest)
        .expect("blake2b-256 output buffer has the requested size");
    Ok(StagedUpload {
        blob: pending.finish().map_err(storage_reject)?,
        blake2_256: hex(&digest),
    })
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

/// Map any multipart read or decode failure to a 400 response.
fn reject(err: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, format!("bad upload: {err}")).into_response()
}

fn storage_reject(err: impl std::fmt::Display) -> Response {
    tracing::error!(error = %err, "upload staging failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("upload staging: blob store error: {err}"),
    )
        .into_response()
}

pub(super) fn upload_error_response(err: &UploadError) -> Response {
    upload_error_message(err).into_response()
}

pub(super) fn upload_error_message(err: &UploadError) -> (StatusCode, String) {
    match err {
        UploadError::NotFileUpload => (StatusCode::BAD_REQUEST, "unsupported :action".to_owned()),
        UploadError::Missing(field) => (StatusCode::BAD_REQUEST, format!("missing required field: {field}")),
        UploadError::InvalidName(name) => (
            StatusCode::BAD_REQUEST,
            format!(
                "invalid project name {name:?}: names must start and end with an ASCII letter or digit and contain only letters, digits, '.', '_' or '-'"
            ),
        ),
        UploadError::InvalidVersion(version) => (
            StatusCode::BAD_REQUEST,
            format!("invalid version {version:?}: expected a PEP 440 version"),
        ),
        UploadError::InvalidFilename(filename) => (
            StatusCode::BAD_REQUEST,
            format!(
                "invalid filename {filename:?}: filenames must be relative path segments without separators, traversal, or control characters"
            ),
        ),
        UploadError::InvalidDistributionFilename { filename, error } => (
            StatusCode::BAD_REQUEST,
            format!(
                "invalid distribution filename {filename:?}: {}",
                distribution_filename_error_message(error)
            ),
        ),
        UploadError::FiletypeMismatch { expected, actual } => (
            StatusCode::BAD_REQUEST,
            format!("filetype {actual:?} does not match filename; expected {expected:?}"),
        ),
        UploadError::FilenameNameMismatch { filename, form } => (
            StatusCode::BAD_REQUEST,
            format!("filename project {filename:?} does not match upload name {form:?}"),
        ),
        UploadError::FilenameVersionMismatch { filename, form } => (
            StatusCode::BAD_REQUEST,
            format!("filename version {filename:?} does not match upload version {form:?}"),
        ),
        UploadError::DigestMismatch(field) => (StatusCode::BAD_REQUEST, format!("{field} mismatch")),
        UploadError::Md5Only => (
            StatusCode::BAD_REQUEST,
            "md5_digest is not accepted without a sha256_digest or blake2_256_digest".to_owned(),
        ),
        UploadError::InvalidDigest { field, value } => (
            StatusCode::BAD_REQUEST,
            format!("{field} value {value:?} is not lowercase hex with the expected length"),
        ),
        UploadError::InvalidRequiresPython(value) => (
            StatusCode::BAD_REQUEST,
            format!("invalid Requires-Python value {value:?}: expected PEP 440 version specifiers"),
        ),
        UploadError::InvalidContent(message) => (
            StatusCode::BAD_REQUEST,
            format!("uploaded content does not match the filename format: {message}"),
        ),
        UploadError::InvalidMetadataUtf8 => (
            StatusCode::BAD_REQUEST,
            "artifact metadata is not valid UTF-8".to_owned(),
        ),
        UploadError::MetadataNameMismatch { metadata, form } => (
            StatusCode::BAD_REQUEST,
            format!("metadata Name {metadata:?} does not match upload name {form:?}"),
        ),
        UploadError::MetadataVersionMismatch { metadata, form } => (
            StatusCode::BAD_REQUEST,
            format!("metadata Version {metadata:?} does not match upload version {form:?}"),
        ),
        UploadError::MetadataFieldMismatch { field, metadata, form } => {
            upload_metadata_field_mismatch_message(field, metadata, form)
        }
        UploadError::InvalidUploadTime => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "configured clock produced an invalid upload timestamp".to_owned(),
        ),
    }
}

fn upload_metadata_field_mismatch_message(field: &str, metadata: &str, form: &str) -> (StatusCode, String) {
    (
        StatusCode::BAD_REQUEST,
        format!("metadata {field} {metadata:?} does not match upload value {form:?}"),
    )
}

fn distribution_filename_error_message(err: &DistributionFilenameError) -> String {
    match err {
        DistributionFilenameError::UnsupportedExtension => "accepted upload formats are .whl and .tar.gz".to_owned(),
        DistributionFilenameError::LegacyEgg => {
            "legacy .egg uploads are not accepted; upload a wheel or .tar.gz sdist".to_owned()
        }
        DistributionFilenameError::InvalidWheelShape => {
            "wheel filenames must use distribution-version(-build tag)?-python tag-abi tag-platform tag.whl".to_owned()
        }
        DistributionFilenameError::InvalidSdistShape => "sdist filenames must use name-version.tar.gz".to_owned(),
        DistributionFilenameError::InvalidName(name) => {
            format!("distribution name component {name:?} is not a valid PyPA project name")
        }
        DistributionFilenameError::InvalidVersion(version) => {
            format!("version component {version:?} is not a PEP 440 version")
        }
        DistributionFilenameError::InvalidTag(tag) => {
            format!("wheel build/tag component {tag:?} contains invalid characters")
        }
    }
}
