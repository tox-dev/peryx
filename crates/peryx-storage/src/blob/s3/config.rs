//! S3 configuration keeps credentials out of parsed settings and debug output.

use std::fmt;
use std::time::Duration;

use url::Url;

use crate::blob::DurabilityCapabilities;

const MIN_PART_SIZE: u64 = 5 << 20;
const MAX_PART_SIZE: u64 = 5 << 30;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum S3ConfigError {
    #[error("s3 bucket must not be empty")]
    EmptyBucket,
    #[error("s3 region must not be empty")]
    EmptyRegion,
    #[error("s3 endpoint is not a valid URL: {reason}")]
    Endpoint { reason: String },
    #[error("s3 endpoint must use http or https")]
    EndpointScheme,
    #[error("s3 endpoint must not contain credentials, a query, or a fragment")]
    EndpointComponents,
    #[error("s3 {field} must be greater than zero")]
    Zero { field: &'static str },
    #[error("s3 part_size must be between 5 MiB and 5 GiB")]
    PartSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum S3Addressing {
    Path,
    VirtualHost,
}

#[derive(Clone)]
pub struct S3Config {
    pub endpoint: Url,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    pub addressing: S3Addressing,
    pub request_timeout: Duration,
    pub max_retries: u32,
    pub multipart_threshold: u64,
    pub part_size: u64,
    pub upload_concurrency: usize,
    /// The endpoint accepts `If-None-Match: *`; operators must declare this for non-AWS endpoints.
    pub conditional_writes: bool,
    /// The endpoint validates the SHA-256 checksum sent with each write.
    pub checksum_writes: bool,
}

impl fmt::Debug for S3Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3Config")
            .field("endpoint", &"<redacted>")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .field("addressing", &self.addressing)
            .field("request_timeout", &self.request_timeout)
            .field("max_retries", &self.max_retries)
            .field("multipart_threshold", &self.multipart_threshold)
            .field("part_size", &self.part_size)
            .field("upload_concurrency", &self.upload_concurrency)
            .field("conditional_writes", &self.conditional_writes)
            .field("checksum_writes", &self.checksum_writes)
            .finish()
    }
}

#[derive(Clone)]
pub struct S3Settings {
    pub endpoint: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    pub path_style: bool,
    pub request_timeout: Duration,
    pub max_retries: u32,
    pub multipart_threshold: u64,
    pub part_size: u64,
    pub upload_concurrency: usize,
    pub conditional_writes: bool,
    pub checksum_writes: bool,
}

impl fmt::Debug for S3Settings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3Settings")
            .field("endpoint", &"<redacted>")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .field("path_style", &self.path_style)
            .field("request_timeout", &self.request_timeout)
            .field("max_retries", &self.max_retries)
            .field("multipart_threshold", &self.multipart_threshold)
            .field("part_size", &self.part_size)
            .field("upload_concurrency", &self.upload_concurrency)
            .field("conditional_writes", &self.conditional_writes)
            .field("checksum_writes", &self.checksum_writes)
            .finish()
    }
}

impl S3Config {
    /// # Errors
    /// Returns [`S3ConfigError`] for an invalid endpoint, identifier, or transfer bound.
    pub fn new(settings: S3Settings) -> Result<Self, S3ConfigError> {
        if settings.bucket.is_empty() {
            return Err(S3ConfigError::EmptyBucket);
        }
        if settings.region.is_empty() {
            return Err(S3ConfigError::EmptyRegion);
        }
        let mut endpoint = Url::parse(&settings.endpoint).map_err(|error| S3ConfigError::Endpoint {
            reason: error.to_string(),
        })?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(S3ConfigError::EndpointScheme);
        }
        if !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(S3ConfigError::EndpointComponents);
        }
        if !endpoint.path().ends_with('/') {
            endpoint.set_path(&format!("{}/", endpoint.path()));
        }
        for (field, value) in [
            ("request_timeout", settings.request_timeout.as_millis()),
            ("multipart_threshold", u128::from(settings.multipart_threshold)),
            ("upload_concurrency", settings.upload_concurrency as u128),
        ] {
            if value == 0 {
                return Err(S3ConfigError::Zero { field });
            }
        }
        if !(MIN_PART_SIZE..=MAX_PART_SIZE.min(usize::MAX as u64)).contains(&settings.part_size) {
            return Err(S3ConfigError::PartSize);
        }
        Ok(Self {
            endpoint,
            bucket: settings.bucket,
            prefix: settings.prefix.trim_matches('/').to_owned(),
            region: settings.region,
            addressing: if settings.path_style {
                S3Addressing::Path
            } else {
                S3Addressing::VirtualHost
            },
            request_timeout: settings.request_timeout,
            max_retries: settings.max_retries,
            multipart_threshold: settings.multipart_threshold,
            part_size: settings.part_size,
            upload_concurrency: settings.upload_concurrency,
            conditional_writes: settings.conditional_writes,
            checksum_writes: settings.checksum_writes,
        })
    }

    #[must_use]
    pub const fn durability(&self) -> DurabilityCapabilities {
        DurabilityCapabilities::object_store(self.conditional_writes, self.checksum_writes)
    }

    #[must_use]
    pub const fn force_path_style(&self) -> bool {
        matches!(self.addressing, S3Addressing::Path)
    }

    #[must_use]
    pub fn key_for(&self, digest: &str) -> String {
        if self.prefix.is_empty() {
            format!("sha256/{digest}")
        } else {
            format!("{}/sha256/{digest}", self.prefix)
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/blob/s3/config/tests.rs"]
mod tests;
