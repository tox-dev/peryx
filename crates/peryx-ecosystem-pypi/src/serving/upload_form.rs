use axum::extract::{Multipart, multipart};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use blake2::Blake2bVar;
use blake2::digest::{Update as _, VariableOutput as _};
use peryx_driver::body::BodyFailure;
use peryx_policy::{PolicyAction, PolicyDenial};

use crate::DistributionFilenameError;
use crate::upload::{StagedUpload, UploadError, UploadForm};

use super::HttpResult;
use super::response::policy_denial_response;

const MAX_UPLOAD_TEXT_FIELD_BYTES: usize = 64 * 1024;
/// One attestation bundle may use 1 MiB; the second MiB leaves the same bounded share for all other
/// retained metadata.
const MAX_UPLOAD_TEXT_BYTES: usize = 2 * 1024 * 1024;
/// Three repeatable metadata fields may each reach their own limit, leaving 64 parts for scalar and
/// ignored fields.
const MAX_UPLOAD_PARTS: usize = 256;
/// A 64-entry cap admits long repeated metadata lists without unbounded vector growth.
const MAX_UPLOAD_REPEATED_FIELDS: usize = 64;

/// The aggregate cap on the PEP 740 `attestations` field. A bundle carries certificates and
/// transparency proofs, so it needs more room than a metadata line, but this bounds what one
/// untrusted field can buffer before parsing splits it into per-attestation limits.
const MAX_ATTESTATIONS_FIELD_BYTES: usize = 1024 * 1024;

/// Drain a multipart body into an [`UploadForm`], staging the `content` part on disk while the rest
/// stays as UTF-8 text. Unknown fields are ignored, as the upload API carries many metadata fields
/// peryx does not need. Every read or decode error funnels through [`reject`] as a 400.
pub(super) async fn collect_form(
    mut multipart: Multipart,
    blobs: &peryx_storage::blob::BlobStorage,
    max_file_size: Option<u64>,
    browser: bool,
) -> HttpResult<(UploadForm, Option<StagedUpload>)> {
    let mut form = UploadForm::default();
    let mut staged = None;
    let mut budget = FormBudget::default();
    while let Some(field) = multipart.next_field().await.map_err(|error| body_reject(&error))? {
        budget.add_part()?;
        let field_name = field.name().unwrap_or_default().to_owned();
        if field_name == "content" {
            if staged.is_some() {
                return Err(reject("duplicate content field").into());
            }
            form.filename = field.file_name().map(str::to_owned);
            if browser {
                complete_browser_form(&mut form).map_err(|err| upload_error_response(&err))?;
            }
            staged = Some(stage_content(field, blobs, max_file_size, &form).await?);
        } else if let Some(upload_field) = upload_text_field(&field_name) {
            budget.add_field(upload_field, &field_name)?;
            let value = read_text_field(field, &field_name, text_field_limit(upload_field), &mut budget).await?;
            if !browser || !browser_derived(upload_field) {
                set_upload_text_field(&mut form, upload_field, value);
            }
        } else {
            drain_field(field, &mut budget).await?;
        }
    }
    Ok((form, staged))
}

#[derive(Default)]
struct FormBudget {
    parts: usize,
    text_bytes: usize,
    fields: [usize; UploadTextField::COUNT],
}

impl FormBudget {
    fn add_part(&mut self) -> HttpResult<()> {
        self.parts += 1;
        if self.parts > MAX_UPLOAD_PARTS {
            return Err(reject(format!("upload has more than {MAX_UPLOAD_PARTS} parts")).into());
        }
        Ok(())
    }

    fn add_field(&mut self, field: UploadTextField, name: &str) -> HttpResult<()> {
        let count = &mut self.fields[field as usize];
        *count += 1;
        let limit = if field.is_repeated() {
            MAX_UPLOAD_REPEATED_FIELDS
        } else {
            1
        };
        if *count <= limit {
            return Ok(());
        }
        let reason = if limit == 1 {
            format!("duplicate upload field {name:?}")
        } else {
            format!("upload field {name:?} appears more than {limit} times")
        };
        Err(reject(reason).into())
    }

    fn add_text(&mut self, bytes: usize) -> HttpResult<()> {
        if self.text_bytes.saturating_add(bytes) > MAX_UPLOAD_TEXT_BYTES {
            return Err(reject(format!("upload text fields exceed {MAX_UPLOAD_TEXT_BYTES} bytes")).into());
        }
        self.text_bytes += bytes;
        Ok(())
    }
}

fn complete_browser_form(form: &mut UploadForm) -> Result<(), UploadError> {
    let filename = form.filename.as_deref().ok_or(UploadError::Missing("filename"))?;
    let parsed =
        crate::parse_distribution_filename(filename).map_err(|error| UploadError::InvalidDistributionFilename {
            filename: filename.to_owned(),
            error,
        })?;
    form.action = Some("file_upload".to_owned());
    form.name = Some(parsed.name);
    form.version = Some(parsed.version.to_string());
    form.filetype = Some(parsed.kind.upload_filetype().to_owned());
    Ok(())
}

#[derive(Clone, Copy)]
#[repr(usize)]
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
    Attestations,
}

impl UploadTextField {
    const COUNT: usize = Self::Attestations as usize + 1;

    const fn is_repeated(self) -> bool {
        matches!(self, Self::LicenseFile | Self::ProvidesExtra | Self::ProjectUrl)
    }
}

const fn browser_derived(field: UploadTextField) -> bool {
    matches!(
        field,
        UploadTextField::Action | UploadTextField::Name | UploadTextField::Version | UploadTextField::Filetype
    )
}

const fn text_field_limit(field: UploadTextField) -> usize {
    match field {
        UploadTextField::Attestations => MAX_ATTESTATIONS_FIELD_BYTES,
        _ => MAX_UPLOAD_TEXT_FIELD_BYTES,
    }
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
        "attestations" => Some(UploadTextField::Attestations),
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
        UploadTextField::Attestations => form.attestations = Some(value),
    }
}

async fn read_text_field(
    mut field: axum::extract::multipart::Field<'_>,
    name: &str,
    limit: usize,
    budget: &mut FormBudget,
) -> HttpResult<String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(|error| body_reject(&error))? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("upload field {name:?} exceeds {limit} bytes"),
            )
                .into_response()
                .into());
        }
        budget.add_text(chunk.len())?;
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|error| reject(error).into())
}

async fn drain_field(mut field: axum::extract::multipart::Field<'_>, budget: &mut FormBudget) -> HttpResult<()> {
    while let Some(chunk) = field.chunk().await.map_err(|error| body_reject(&error))? {
        budget.add_text(chunk.len())?;
    }
    Ok(())
}

async fn stage_content(
    mut field: axum::extract::multipart::Field<'_>,
    blobs: &peryx_storage::blob::BlobStorage,
    max_file_size: Option<u64>,
    form: &UploadForm,
) -> HttpResult<StagedUpload> {
    let limit = max_file_size.unwrap_or(u64::MAX);
    if let Some(size) = field
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        && size > limit
    {
        return Err(upload_size_reject(form, size, limit).into());
    }
    let mut pending = blobs.begin().await.map_err(storage_reject)?;
    let mut blake2 = Blake2bVar::new(32).expect("blake2b-256 output size is valid");
    let mut size = 0_u64;
    loop {
        let chunk = match field.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                let response = reject(error);
                pending.abort().await.map_err(storage_reject)?;
                return Err(response.into());
            }
        };
        size = size.saturating_add(chunk.len() as u64);
        if size > limit {
            let response = upload_size_reject(form, size, limit);
            pending.abort().await.map_err(storage_reject)?;
            return Err(response.into());
        }
        blake2.update(&chunk);
        pending.write_chunk(chunk).await.map_err(storage_reject)?;
    }
    let mut digest = [0; 32];
    blake2
        .finalize_variable(&mut digest)
        .expect("blake2b-256 output buffer has the requested size");
    Ok(StagedUpload {
        blob: pending.finish().await.map_err(storage_reject)?,
        blake2_256: hex(&digest),
    })
}

fn upload_size_reject(form: &UploadForm, size: u64, limit: u64) -> Response {
    let project = form.name.as_deref().map(crate::normalize_name).unwrap_or_default();
    policy_denial_response(&PolicyDenial::new(
        PolicyAction::Upload,
        &project,
        form.filename.as_deref(),
        form.version.clone(),
        "max-file-size",
        "size",
        format!("file size {size} exceeds limit {limit}"),
    ))
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

/// A read of the multipart body that failed.
///
/// The bytes come from the client either way, so the only question is whether it stopped sending or
/// sent something the server could not read. `BodyFailure` answers that once for every ecosystem
/// instead of each guessing from the message, and the two answers differ for the client: a stall says
/// nothing about the form, so repeating the upload is the right move, while a malformed form repeated
/// unchanged fails again.
fn body_reject(error: &multipart::MultipartError) -> Response {
    match BodyFailure::of(error) {
        BodyFailure::Stalled(after) => (
            StatusCode::REQUEST_TIMEOUT,
            format!("upload stopped: the request body sent nothing for {after:?}"),
        )
            .into_response(),
        BodyFailure::Interrupted => reject(error),
    }
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
        UploadError::MalformedMetadata(err) => (StatusCode::BAD_REQUEST, format!("malformed artifact metadata: {err}")),
        UploadError::InvalidProjectUrl { label, url } => (
            StatusCode::BAD_REQUEST,
            format!(
                "invalid metadata Project-URL label {label:?} with URL {url:?}: expected a label of 1 to 32 characters and an HTTP or HTTPS URL"
            ),
        ),
        UploadError::InvalidLicenseFile { value, reason } => (
            StatusCode::BAD_REQUEST,
            format!("invalid metadata License-File {value:?}: {reason}"),
        ),
        UploadError::ConflictingLicenseFields => (
            StatusCode::BAD_REQUEST,
            "metadata License and License-Expression fields are mutually exclusive".to_owned(),
        ),
        UploadError::MissingMetadataVersion => (
            StatusCode::BAD_REQUEST,
            "artifact metadata is missing required Metadata-Version".to_owned(),
        ),
        UploadError::UnsupportedMetadataVersion(value) => unsupported_metadata_version_message(value),
        UploadError::InvalidMetadataValue { field, value, reason } => (
            StatusCode::BAD_REQUEST,
            format!("metadata {field} value {value:?} {reason}"),
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
        UploadError::Attestation(error) => (StatusCode::BAD_REQUEST, error.message()),
    }
}

fn unsupported_metadata_version_message(value: &str) -> (StatusCode, String) {
    (
        StatusCode::BAD_REQUEST,
        format!(
            "invalid metadata Metadata-Version {value:?}: supported values are 1.0 through 1.2 and 2.1 through 2.6"
        ),
    )
}

fn upload_metadata_field_mismatch_message(field: &str, metadata: &str, form: &str) -> (StatusCode, String) {
    (
        StatusCode::BAD_REQUEST,
        format!("metadata {field} {metadata:?} does not match upload value {form:?}"),
    )
}

fn distribution_filename_error_message(err: &DistributionFilenameError) -> String {
    match err {
        DistributionFilenameError::UnsupportedExtension => {
            "accepted upload formats are .whl, .tar.gz, and .zip".to_owned()
        }
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
