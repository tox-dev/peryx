use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig, SupportedProtocolVersion};

pub(super) struct TestPki {
    ca_pem: String,
    ca_der: CertificateDer<'static>,
    client_cert_pem: String,
    client_key_pem: String,
    server_cert: CertificateDer<'static>,
    server_key: PrivatePkcs8KeyDer<'static>,
}

impl TestPki {
    pub(super) fn new() -> Self {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::CrlSign,
        ];
        let ca = ca_params.self_signed(&ca_key).unwrap();

        let server_key = KeyPair::generate().unwrap();
        let mut server_params = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server = server_params.signed_by(&server_key, &ca, &ca_key).unwrap();

        let client_key = KeyPair::generate().unwrap();
        let mut client_params = CertificateParams::default();
        client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client = client_params.signed_by(&client_key, &ca, &ca_key).unwrap();

        Self {
            ca_pem: ca.pem(),
            ca_der: ca.der().clone(),
            client_cert_pem: client.pem(),
            client_key_pem: client_key.serialize_pem(),
            server_cert: server.der().clone(),
            server_key: PrivatePkcs8KeyDer::from(server_key.serialize_der()),
        }
    }

    pub(super) fn write_client_files(&self) -> TlsFiles {
        let dir = tempfile::tempdir().unwrap();
        let files = TlsFiles {
            ca: dir.path().join("ca.pem"),
            certificate: dir.path().join("client.pem"),
            key: dir.path().join("client-key.pem"),
            _dir: dir,
        };
        std::fs::write(&files.ca, &self.ca_pem).unwrap();
        std::fs::write(&files.certificate, &self.client_cert_pem).unwrap();
        std::fs::write(&files.key, &self.client_key_pem).unwrap();
        files
    }

    pub(super) fn start_server(&self, router: Router) -> TlsServer {
        self.start_server_with_options(router, &[&rustls::version::TLS13], true)
    }

    pub(super) fn start_server_without_client_identity(&self, router: Router) -> TlsServer {
        self.start_server_with_options(router, &[&rustls::version::TLS13], false)
    }

    pub(super) fn start_tls12_server(&self, router: Router) -> TlsServer {
        self.start_server_with_options(router, &[&rustls::version::TLS12], true)
    }

    fn start_server_with_options(
        &self,
        router: Router,
        versions: &[&'static SupportedProtocolVersion],
        require_client_identity: bool,
    ) -> TlsServer {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = RootCertStore::empty();
        roots.add(self.ca_der.clone()).unwrap();
        let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
            .build()
            .unwrap();
        let builder = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .unwrap();
        let builder = if require_client_identity {
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };
        let mut config = builder
            .with_single_cert(
                vec![self.server_cert.clone()],
                PrivateKeyDer::Pkcs8(self.server_key.clone_key()),
            )
            .unwrap();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum_server::from_tcp_rustls(
                listener,
                axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(config)),
            )
            .unwrap()
            .serve(router.into_make_service())
            .await
            .unwrap();
        });
        TlsServer {
            base: format!("https://{address}"),
            task,
        }
    }
}

pub(super) struct TlsFiles {
    _dir: tempfile::TempDir,
    pub(super) ca: PathBuf,
    pub(super) certificate: PathBuf,
    pub(super) key: PathBuf,
}

pub(super) struct TlsServer {
    base: String,
    task: tokio::task::JoinHandle<()>,
}

impl TlsServer {
    pub(super) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
