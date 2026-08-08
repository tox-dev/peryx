use peryx_upstream::{Auth, UpstreamClient, UpstreamError, UpstreamTls};
use rstest::rstest;

use super::tls_support::TestPki;

#[test]
fn test_upstream_tls_file_errors_are_redacted() {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let missing = files.ca.with_file_name("secret-ca-name.pem");
    let error = UpstreamTls::from_paths(Some(&missing), None).unwrap_err();

    assert_eq!(error.to_string(), "cannot read upstream CA bundle");
    assert!(!format!("{error:?}").contains("secret-ca-name"));
}

#[test]
fn test_upstream_tls_debug_reports_only_configuration_shape() {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let tls = UpstreamTls::from_paths(Some(&files.ca), Some((&files.certificate, &files.key))).unwrap();

    assert_eq!(
        format!("{tls:?}"),
        "UpstreamTls { custom_ca: true, client_identity: true }"
    );
}

#[rstest]
#[case::certificate(true, "cannot read upstream client certificate")]
#[case::key(false, "cannot read upstream client private key")]
fn test_upstream_tls_reports_unreadable_identity_file(#[case] certificate: bool, #[case] expected: &str) {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let missing_certificate = files.ca.with_file_name("missing-certificate.pem");
    let missing_key = files.ca.with_file_name("missing-key.pem");
    let identity = if certificate {
        (missing_certificate.as_path(), files.key.as_path())
    } else {
        (files.certificate.as_path(), missing_key.as_path())
    };

    assert_eq!(
        UpstreamTls::from_paths(Some(&files.ca), Some(identity))
            .unwrap_err()
            .to_string(),
        expected
    );
}

#[rstest]
#[case::empty("", "upstream CA bundle contains no certificates")]
#[case::invalid(
    "-----BEGIN CERTIFICATE-----\nnot-base64\n-----END CERTIFICATE-----\n",
    "upstream CA bundle has invalid PEM certificates"
)]
fn test_upstream_tls_rejects_invalid_ca(#[case] contents: &str, #[case] expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, contents).unwrap();

    assert_eq!(
        UpstreamTls::from_paths(Some(&ca), None).unwrap_err().to_string(),
        expected
    );
}

#[test]
fn test_upstream_tls_rejects_invalid_ca_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n").unwrap();
    let tls = UpstreamTls::from_paths(Some(&ca), None).unwrap();
    let error = UpstreamClient::with_auth_and_tls("https://example.invalid/", Auth::None, &tls).unwrap_err();

    assert!(matches!(error, UpstreamError::Http(_)));
}

#[test]
fn test_upstream_tls_rejects_invalid_client_identity() {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    std::fs::write(&files.certificate, "not a certificate").unwrap();

    assert_eq!(
        UpstreamTls::from_paths(Some(&files.ca), Some((&files.certificate, &files.key)))
            .unwrap_err()
            .to_string(),
        "upstream client certificate or private key has invalid PEM"
    );
}

#[test]
fn test_upstream_tls_rejects_mismatched_client_key() {
    let pki = TestPki::new();
    let files = pki.write_client_files();
    let other_files = TestPki::new().write_client_files();
    std::fs::copy(&other_files.key, &files.key).unwrap();

    let tls = UpstreamTls::from_paths(Some(&files.ca), Some((&files.certificate, &files.key))).unwrap();
    let error = UpstreamClient::with_auth_and_tls("https://example.invalid/", Auth::None, &tls).unwrap_err();

    assert!(matches!(error, UpstreamError::Http(_)));
}
