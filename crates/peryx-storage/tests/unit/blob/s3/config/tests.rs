use std::time::Duration;

use rstest::rstest;

use super::{S3Addressing, S3Config, S3ConfigError, S3Settings};
use crate::blob::DurabilityCapabilities;

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
        conditional_writes: true,
        checksum_writes: true,
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

#[rstest]
#[case::verified(true, true)]
#[case::no_conditional(false, true)]
#[case::no_checksum(true, false)]
#[case::basic(false, false)]
fn test_config_reports_declared_durability(#[case] conditional_writes: bool, #[case] checksum_writes: bool) {
    let config = S3Config::new(S3Settings {
        conditional_writes,
        checksum_writes,
        ..settings()
    })
    .unwrap();
    assert_eq!(
        config.durability(),
        DurabilityCapabilities::object_store(conditional_writes, checksum_writes)
    );
}
