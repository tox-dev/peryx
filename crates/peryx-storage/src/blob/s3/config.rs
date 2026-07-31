//! Secret-free S3 endpoint and transfer bounds.

use std::fmt;
use std::time::Duration;

use url::Url;

const MIN_PART_SIZE: u64 = 5 << 20;
const MAX_PART_SIZE: u64 = 5 << 30;

/// A configuration value that cannot describe a usable S3 backend.
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

/// The addressing style an endpoint expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum S3Addressing {
    Path,
    VirtualHost,
}

/// Connection and transfer settings for one bucket.
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
            .finish()
    }
}

/// Raw settings supplied by configuration.
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
            .finish()
    }
}

impl S3Config {
    /// Validate raw settings.
    ///
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
        })
    }

    #[must_use]
    pub const fn force_path_style(&self) -> bool {
        matches!(self.addressing, S3Addressing::Path)
    }

    /// Map a digest to its immutable object key.
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
mod tests {
    use std::time::Duration;

    use rstest::rstest;

    use super::{S3Addressing, S3Config, S3ConfigError, S3Settings};

    fn settings() -> S3Settings {
        S3Settings {
            endpoint: "https://s3.example.com/base".to_owned(),
            bucket: "bucket".to_owned(),
            prefix: "/cache/".to_owned(),
            region: "us-east-1".to_owned(),
            path_style: true,
            request_timeout: Duration::from_secs(30),
            max_retries: 3,
            multipart_threshold: 16 << 20,
            part_size: 8 << 20,
            upload_concurrency: 4,
        }
    }

    #[test]
    fn test_config_maps_endpoint_prefix_and_addressing() {
        let config = S3Config::new(settings()).unwrap();
        assert_eq!(config.endpoint.as_str(), "https://s3.example.com/base/");
        assert!(!format!("{config:?}").contains("/base"));
        assert_eq!(config.prefix, "cache");
        assert_eq!(config.addressing, S3Addressing::Path);
        assert!(config.force_path_style());
        assert_eq!(config.key_for("abcd"), "cache/sha256/abcd");
    }

    #[test]
    fn test_config_maps_root_prefix_and_virtual_host_addressing() {
        let config = S3Config::new(S3Settings {
            prefix: String::new(),
            path_style: false,
            ..settings()
        })
        .unwrap();
        assert_eq!(config.addressing, S3Addressing::VirtualHost);
        assert!(!config.force_path_style());
        assert_eq!(config.key_for("abcd"), "sha256/abcd");
    }

    #[rstest]
    #[case::bucket(
        S3Settings { bucket: String::new(), ..settings() },
        S3ConfigError::EmptyBucket
    )]
    #[case::region(
        S3Settings { region: String::new(), ..settings() },
        S3ConfigError::EmptyRegion
    )]
    #[case::scheme(
        S3Settings { endpoint: "ftp://s3.example.com".to_owned(), ..settings() },
        S3ConfigError::EndpointScheme
    )]
    #[case::timeout(
        S3Settings { request_timeout: Duration::ZERO, ..settings() },
        S3ConfigError::Zero { field: "request_timeout" }
    )]
    #[case::threshold(
        S3Settings { multipart_threshold: 0, ..settings() },
        S3ConfigError::Zero { field: "multipart_threshold" }
    )]
    #[case::concurrency(
        S3Settings { upload_concurrency: 0, ..settings() },
        S3ConfigError::Zero { field: "upload_concurrency" }
    )]
    #[case::small_part(S3Settings { part_size: (5 << 20) - 1, ..settings() }, S3ConfigError::PartSize)]
    #[case::large_part(S3Settings { part_size: (5 << 30) + 1, ..settings() }, S3ConfigError::PartSize)]
    fn test_config_rejects_invalid_values(#[case] input: S3Settings, #[case] expected: S3ConfigError) {
        assert_eq!(S3Config::new(input).unwrap_err(), expected);
    }

    #[test]
    fn test_config_rejects_an_unparsable_endpoint() {
        let secret = "not a url secret";
        let settings = S3Settings {
            endpoint: secret.to_owned(),
            ..settings()
        };
        assert!(!format!("{settings:?}").contains(secret));
        let error = S3Config::new(settings).unwrap_err();
        assert_eq!(
            error,
            S3ConfigError::Endpoint {
                reason: "relative URL without a base".to_owned()
            }
        );
        assert!(!error.to_string().contains(secret));
    }

    #[rstest]
    #[case::username("https://access@s3.example.com/base")]
    #[case::password("https://access:secret@s3.example.com/base")]
    #[case::query("https://s3.example.com/base?token=secret")]
    #[case::fragment("https://s3.example.com/base#secret")]
    fn test_config_rejects_secret_bearing_endpoint_components(#[case] endpoint: &str) {
        let error = S3Config::new(S3Settings {
            endpoint: endpoint.to_owned(),
            ..settings()
        })
        .unwrap_err();
        assert_eq!(error, S3ConfigError::EndpointComponents);
        assert!(!error.to_string().contains("secret"));
    }
}
