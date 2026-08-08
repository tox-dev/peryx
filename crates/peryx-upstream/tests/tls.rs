use std::fs;

use peryx_upstream::{Auth, UpstreamClient, UpstreamTls};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rstest::rstest;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn test_tls_defaults_to_platform_trust_without_an_identity() {
    let tls = UpstreamTls::from_paths(None, None).unwrap();

    assert_eq!(
        format!("{tls:?}"),
        "UpstreamTls { custom_ca: false, client_identity: false }"
    );
}

#[test]
fn test_tls_loads_a_ca_and_client_identity() {
    let (_directory, tls) = tls_material(true);

    assert_eq!(
        format!("{tls:?}"),
        "UpstreamTls { custom_ca: true, client_identity: true }"
    );
    assert!(UpstreamClient::with_auth_and_tls("https://upstream.example/simple/", Auth::None, &tls).is_ok());
}

#[rstest]
#[case::missing(None, "cannot read upstream CA bundle")]
#[case::malformed(
    Some(b"-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n".as_slice()),
    "upstream CA bundle has invalid PEM certificates"
)]
#[case::empty(Some(b"".as_slice()), "upstream CA bundle contains no certificates")]
fn test_tls_rejects_invalid_ca_files(#[case] contents: Option<&[u8]>, #[case] expected: &str) {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("ca.pem");
    if let Some(contents) = contents {
        fs::write(&path, contents).unwrap();
    }

    assert_eq!(
        UpstreamTls::from_paths(Some(&path), None).unwrap_err().to_string(),
        expected
    );
}

#[rstest]
#[case::missing_certificate(false, true, false, "cannot read upstream client certificate")]
#[case::missing_key(true, false, false, "cannot read upstream client private key")]
#[case::invalid_identity(true, true, true, "upstream client certificate or private key has invalid PEM")]
fn test_tls_rejects_invalid_identity_files(
    #[case] write_certificate: bool,
    #[case] write_key: bool,
    #[case] corrupt_certificate: bool,
    #[case] expected: &str,
) {
    let directory = TempDir::new().unwrap();
    let certificate_path = directory.path().join("client.crt");
    let key_path = directory.path().join("client.key");
    let (certificate, key) = identity_material();
    if write_certificate {
        fs::write(
            &certificate_path,
            if corrupt_certificate { "invalid" } else { &certificate },
        )
        .unwrap();
    }
    if write_key {
        fs::write(&key_path, key).unwrap();
    }

    assert_eq!(
        UpstreamTls::from_paths(None, Some((&certificate_path, &key_path)))
            .unwrap_err()
            .to_string(),
        expected
    );
}

#[tokio::test]
async fn test_client_identity_refuses_a_cross_origin_redirect() {
    let origin = MockServer::start().await;
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/artifact"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/artifact", target.uri())))
        .mount(&origin)
        .await;
    let (_directory, tls) = tls_material(false);
    let client = UpstreamClient::with_auth_and_tls(&origin.uri(), Auth::None, &tls).unwrap();

    assert_eq!(
        (
            client
                .fetch_bytes(&format!("{}/artifact", origin.uri()))
                .await
                .unwrap_err()
                .user_message(),
            target.received_requests().await.unwrap().len(),
        ),
        ("upstream request failed".to_owned(), 0)
    );
}

fn tls_material(include_ca: bool) -> (TempDir, UpstreamTls) {
    let directory = TempDir::new().unwrap();
    let certificate_path = directory.path().join("client.crt");
    let key_path = directory.path().join("client.key");
    let (certificate, key) = identity_material();
    fs::write(&certificate_path, &certificate).unwrap();
    fs::write(&key_path, key).unwrap();
    let ca_path = directory.path().join("ca.pem");
    if include_ca {
        fs::write(&ca_path, certificate).unwrap();
    }
    let tls = UpstreamTls::from_paths(
        include_ca.then_some(ca_path.as_path()),
        Some((&certificate_path, &key_path)),
    )
    .unwrap();
    (directory, tls)
}

fn identity_material() -> (String, String) {
    let key = KeyPair::generate().unwrap();
    let mut parameters = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    (parameters.self_signed(&key).unwrap().pem(), key.serialize_pem())
}
