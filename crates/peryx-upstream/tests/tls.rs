use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use peryx_upstream::{Auth, UpstreamClient, UpstreamTls};
use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
use rstest::rstest;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn test_tls_debug_reports_material_presence_without_contents() {
    let fixture = tls_fixture(true, true);

    assert_eq!(
        format!("{:?}", fixture.tls),
        "UpstreamTls { custom_ca: true, client_identity: true }"
    );
}

#[tokio::test]
async fn test_tls_uses_the_custom_ca_and_client_identity() {
    let fixture = tls_fixture(true, true);
    let server = MtlsServer::start(fixture.server);
    let client = UpstreamClient::with_auth_and_tls(&server.base, Auth::None, &fixture.tls).unwrap();
    let bytes = client.fetch_bytes(&format!("{}artifact", server.base)).await.unwrap();

    assert_eq!((&bytes[..], server.join()), (b"secured".as_slice(), true));
}

#[rstest]
#[case::without_custom_ca(false, true, "upstream connection failed")]
#[case::without_client_identity(true, false, "upstream request failed")]
#[tokio::test]
async fn test_tls_rejects_connections_missing_required_material(
    #[case] include_ca: bool,
    #[case] include_identity: bool,
    #[case] expected: &str,
) {
    let fixture = tls_fixture(include_ca, include_identity);
    let server = MtlsServer::start(fixture.server);
    let client = UpstreamClient::with_auth_and_tls(&server.base, Auth::None, &fixture.tls).unwrap();
    let error = client
        .fetch_bytes(&format!("{}artifact", server.base))
        .await
        .unwrap_err();

    assert_eq!((error.user_message(), server.join()), (expected.to_owned(), false));
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
    let fixture = tls_fixture(false, true);
    let client = UpstreamClient::with_auth_and_tls(&origin.uri(), Auth::None, &fixture.tls).unwrap();

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

fn tls_fixture(include_ca: bool, include_identity: bool) -> TlsFixture {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_parameters = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_parameters
        .distinguished_name
        .push(DnType::CommonName, "peryx test root");
    ca_parameters.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca_parameters.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let ca = ca_parameters.self_signed(&ca_key).unwrap();
    let client_key = KeyPair::generate().unwrap();
    let mut client_parameters = CertificateParams::new(Vec::<String>::new()).unwrap();
    client_parameters
        .distinguished_name
        .push(DnType::CommonName, "peryx test client");
    client_parameters.use_authority_key_identifier_extension = true;
    client_parameters.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    client_parameters.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client = client_parameters.signed_by(&client_key, &ca, &ca_key).unwrap();
    let server_key = KeyPair::generate().unwrap();
    let mut server_parameters = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    server_parameters
        .distinguished_name
        .push(DnType::CommonName, "peryx test server");
    server_parameters.use_authority_key_identifier_extension = true;
    server_parameters.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    server_parameters.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_certificate = server_parameters.signed_by(&server_key, &ca, &ca_key).unwrap();
    let directory = TempDir::new().unwrap();
    let certificate_path = directory.path().join("client.crt");
    let key_path = directory.path().join("client.key");
    fs::write(&certificate_path, client.pem()).unwrap();
    fs::write(&key_path, client_key.serialize_pem()).unwrap();
    let ca_path = directory.path().join("ca.pem");
    fs::write(&ca_path, ca.pem()).unwrap();
    let tls = UpstreamTls::from_paths(
        include_ca.then_some(ca_path.as_path()),
        include_identity.then_some((certificate_path.as_path(), key_path.as_path())),
    )
    .unwrap();
    let mut client_roots = RootCertStore::empty();
    client_roots.add(ca.der().clone()).unwrap();
    let verifier = WebPkiClientVerifier::builder(Arc::new(client_roots)).build().unwrap();
    let server = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![server_certificate.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
        )
        .unwrap();

    TlsFixture {
        _directory: directory,
        tls,
        server: Arc::new(server),
    }
}

fn identity_material() -> (String, String) {
    let key = KeyPair::generate().unwrap();
    let mut parameters = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    (parameters.self_signed(&key).unwrap().pem(), key.serialize_pem())
}

struct TlsFixture {
    _directory: TempDir,
    tls: UpstreamTls,
    server: Arc<ServerConfig>,
}

struct MtlsServer {
    base: String,
    address: SocketAddr,
    done: Arc<AtomicBool>,
    thread: JoinHandle<Result<(), Box<dyn Error + Send + Sync>>>,
}

impl MtlsServer {
    fn start(config: Arc<ServerConfig>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("https://127.0.0.1:{}/", address.port());
        let done = Arc::new(AtomicBool::new(false));
        let server_done = Arc::clone(&done);
        let handle = thread::spawn(move || serve(&listener, &config, &server_done));
        Self {
            base,
            address,
            done,
            thread: handle,
        }
    }

    fn join(self) -> bool {
        self.done.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        self.thread.join().unwrap().is_ok()
    }
}

fn serve(
    listener: &TcpListener,
    config: &Arc<ServerConfig>,
    done: &AtomicBool,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (mut socket, _) = listener.accept()?;
    loop {
        let error = match serve_connection(socket, config) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        socket = listener.accept()?.0;
        if done.load(Ordering::Acquire) {
            return Err(error);
        }
    }
}

fn serve_connection(socket: TcpStream, config: &Arc<ServerConfig>) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut stream = StreamOwned::new(ServerConnection::new(Arc::clone(config))?, socket);
    let mut request = Vec::new();
    let mut byte = [0];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte)?;
        request.push(byte[0]);
    }
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nsecured")?;
    stream.flush()?;
    Ok(())
}
