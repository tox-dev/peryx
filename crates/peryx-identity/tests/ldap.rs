use std::fmt::Write as _;
use std::io::Write as _;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use peryx_identity::{
    ExternalGroupGrant, ExternalIdentityResolution, ExternalLinkRequest, ExternalLogin, GrantScope, LdapBindMode,
    LdapLoginError, LdapLoginService, LdapProvider, LdapProviderError, LdapProviderSettings, MAX_EXTERNAL_GROUPS,
    ProviderId, Role, ServerUser, UserId, UserName, UserState,
};
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
use rstest::rstest;
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject as _};
use url::Url;

const ADMIN_DN: &str = "cn=admin,dc=localhost";
const PEOPLE_DN: &str = "ou=people,dc=localhost";
const FRY_DN: &str = "cn=Philip J. Fry,ou=people,dc=localhost";
const BENDER_DN: &str = "cn=Bender Bending Rodriguez,ou=people,dc=localhost";

#[tokio::test]
async fn test_ldap_login_crosses_starttls_bind_and_store_boundaries() {
    let server = TestLdapServer::start();
    let port = server.port();
    let certificate = server.ca().to_vec();
    let search_provider = LdapProvider::new(openldap_settings(port, certificate.clone(), search_bind())).unwrap();
    let direct_provider = LdapProvider::new(openldap_settings(
        port,
        certificate.clone(),
        LdapBindMode::Direct {
            dn_attribute: "cn".to_owned(),
        },
    ))
    .unwrap();
    let fry = search_provider.authenticate("fry", "fry").await.unwrap().unwrap();

    assert_eq!(fry.display_name.display(), "Fry");
    assert!(
        fry.groups
            .iter()
            .any(|group| group.as_str().starts_with("cn=ship_crew,"))
    );
    assert_eq!(search_provider.authenticate("fry", "wrong").await.unwrap(), None);
    assert_eq!(search_provider.authenticate("fry)(uid=*)", "fry").await.unwrap(), None);
    assert_eq!(
        search_provider
            .authenticate("bender", "bender")
            .await
            .unwrap()
            .unwrap()
            .display_name
            .display(),
        "Bender"
    );
    assert_eq!(
        direct_provider.authenticate("Philip J. Fry", "wrong").await.unwrap(),
        None
    );
    let direct = direct_provider
        .authenticate("Philip J. Fry", "fry")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(direct.identity.subject, fry.identity.subject);

    assert_service_link(search_provider.clone(), &fry).await;
    assert_invalid_entries(port, &certificate).await;
    assert_directory_errors(port, &certificate).await;
    assert_store_error(port, certificate).await;

    drop(direct_provider);
    drop(search_provider);
    drop(server);
}

async fn assert_service_link(search_provider: LdapProvider, fry: &ExternalLogin) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let stable_user = UserId::random();
    let returned_user = stable_user.clone();
    let service = LdapLoginService::new(
        search_provider,
        move |request: ExternalLinkRequest| -> Result<ExternalIdentityResolution, std::convert::Infallible> {
            captured.lock().unwrap().push(request);
            Ok(ExternalIdentityResolution {
                user: ServerUser {
                    id: returned_user.clone(),
                    name: UserName::new("Fry").unwrap(),
                    state: UserState::Active,
                    revision: 1,
                },
                link_created: false,
                grants_changed: false,
            })
        },
        vec![ExternalGroupGrant {
            group: fry
                .groups
                .iter()
                .find(|group| group.as_str().starts_with("cn=ship_crew,"))
                .unwrap()
                .clone(),
            role: Role::RepositoryReader,
            scope: GrantScope::Repository {
                name: "shared-data".to_owned(),
            },
        }],
    );
    assert_eq!(service.id(), &fry.identity.provider);

    assert_eq!(service.authenticate("fry", "wrong").await.unwrap(), None);
    assert_eq!(requests.lock().unwrap().len(), 0);
    let resolution = service.authenticate("fry", "fry").await.unwrap().unwrap();
    assert_eq!(resolution.user.id, stable_user);
    let request = requests.lock().unwrap().pop().unwrap();
    assert_eq!(request.identity.subject, fry.identity.subject);
    assert_eq!(request.grants.len(), 1);
    assert_eq!(request.grants[0].role, Role::RepositoryReader);
}

async fn assert_invalid_entries(port: u16, certificate: &[u8]) {
    for (subject, display, group) in [
        ("employeeNumber", "displayName", Some("memberOf")),
        ("title", "displayName", Some("memberOf")),
        ("entryUUID", "title", Some("memberOf")),
        ("entryUUID", "displayName", Some("title")),
        ("entryUUID", "displayName", Some("description")),
    ] {
        let mut invalid = openldap_settings(port, certificate.to_vec(), search_bind());
        invalid.subject_attribute = subject.to_owned();
        invalid.display_name_attribute = display.to_owned();
        invalid.group_attribute = group.map(str::to_owned);
        assert_eq!(
            LdapProvider::new(invalid)
                .unwrap()
                .authenticate("fry", "fry")
                .await
                .unwrap_err(),
            LdapProviderError::InvalidEntry
        );
    }
    let mut no_groups = openldap_settings(port, certificate.to_vec(), search_bind());
    no_groups.group_attribute = None;
    assert!(
        LdapProvider::new(no_groups)
            .unwrap()
            .authenticate("fry", "fry")
            .await
            .unwrap()
            .unwrap()
            .groups
            .is_empty()
    );
}

async fn assert_directory_errors(port: u16, certificate: &[u8]) {
    let ambiguous = openldap_settings(
        port,
        certificate.to_vec(),
        LdapBindMode::Search {
            username_attribute: "objectClass".to_owned(),
            bind_dn: ADMIN_DN.to_owned(),
            bind_password: "GoodNewsEveryone".to_owned(),
        },
    );
    assert_eq!(
        LdapProvider::new(ambiguous)
            .unwrap()
            .authenticate("inetOrgPerson", "irrelevant")
            .await
            .unwrap_err(),
        LdapProviderError::AmbiguousUser
    );
    let mut invalid_search = openldap_settings(port, certificate.to_vec(), search_bind());
    "not-a-dn".clone_into(&mut invalid_search.base_dn);
    assert_eq!(
        LdapProvider::new(invalid_search)
            .unwrap()
            .authenticate("fry", "fry")
            .await
            .unwrap_err(),
        LdapProviderError::Unavailable
    );
    let mut rejected_service = openldap_settings(port, certificate.to_vec(), search_bind());
    rejected_service.bind = LdapBindMode::Search {
        username_attribute: "uid".to_owned(),
        bind_dn: ADMIN_DN.to_owned(),
        bind_password: "wrong".to_owned(),
    };
    assert_eq!(
        LdapProvider::new(rejected_service)
            .unwrap()
            .authenticate("fry", "fry")
            .await
            .unwrap_err(),
        LdapProviderError::Unavailable
    );
    let mut invalid_direct = openldap_settings(
        port,
        certificate.to_vec(),
        LdapBindMode::Direct {
            dn_attribute: "uid".to_owned(),
        },
    );
    "not-a-dn".clone_into(&mut invalid_direct.base_dn);
    assert_eq!(
        LdapProvider::new(invalid_direct)
            .unwrap()
            .authenticate("fry", "fry")
            .await
            .unwrap_err(),
        LdapProviderError::Unavailable
    );
}

async fn assert_store_error(port: u16, certificate: Vec<u8>) {
    let service = LdapLoginService::new(
        LdapProvider::new(openldap_settings(port, certificate, search_bind())).unwrap(),
        |_request: ExternalLinkRequest| -> Result<ExternalIdentityResolution, &'static str> { Err("write failed") },
        Vec::new(),
    );
    assert_eq!(
        service.authenticate("fry", "fry").await.unwrap_err(),
        LdapLoginError::Store("write failed")
    );
}

fn search_bind() -> LdapBindMode {
    LdapBindMode::Search {
        username_attribute: "uid".to_owned(),
        bind_dn: ADMIN_DN.to_owned(),
        bind_password: "GoodNewsEveryone".to_owned(),
    }
}

fn openldap_settings(port: u16, certificate: Vec<u8>, bind: LdapBindMode) -> LdapProviderSettings {
    LdapProviderSettings {
        id: ProviderId::new("openldap").unwrap(),
        url: Url::parse(&format!("ldap://localhost:{port}")).unwrap(),
        base_dn: PEOPLE_DN.to_owned(),
        bind,
        subject_attribute: "entryUUID".to_owned(),
        display_name_attribute: "displayName".to_owned(),
        group_attribute: Some("memberOf".to_owned()),
        custom_ca_pem: Some(certificate),
        connect_timeout: Duration::from_secs(3),
        request_timeout: Duration::from_secs(5),
        max_connections: NonZeroU32::new(1).unwrap(),
    }
}

struct TestLdapServer {
    port: u16,
    ca: Vec<u8>,
    guard: LdapServerGuard,
}

struct LdapServerGuard {
    port: u16,
    stop: mpsc::Sender<()>,
    sockets: Arc<Mutex<Vec<TcpStream>>>,
    listener: Option<thread::JoinHandle<()>>,
}

impl TestLdapServer {
    fn start() -> Self {
        let (ca, certificate, key) = certificates();
        let config = Arc::new(server_config(&certificate, &key).unwrap());
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        let (stop, stopped) = mpsc::channel();
        let (ready, readiness) = mpsc::sync_channel(0);
        let sockets = Arc::new(Mutex::new(Vec::new()));
        let active_sockets = Arc::clone(&sockets);
        let listener = thread::spawn(move || {
            let mut workers = Vec::new();
            ready.send(()).unwrap();
            loop {
                let (stream, _) = socket.accept().unwrap();
                if stopped.try_recv().is_ok() {
                    break;
                }
                active_sockets.lock().unwrap().push(stream.try_clone().unwrap());
                let config = Arc::clone(&config);
                workers.push(thread::spawn(move || handle_connection(stream, config)));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        readiness.recv().unwrap();
        Self {
            port,
            ca,
            guard: LdapServerGuard {
                port,
                stop,
                sockets,
                listener: Some(listener),
            },
        }
    }

    const fn port(&self) -> u16 {
        self.port
    }

    fn ca(&self) -> &[u8] {
        &self.ca
    }
}

impl Drop for TestLdapServer {
    fn drop(&mut self) {
        self.guard.stop();
    }
}

impl LdapServerGuard {
    fn stop(&mut self) {
        let listener = self.listener.take().unwrap();
        let _ = self.stop.send(());
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        for socket in self
            .sockets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
        let result = listener.join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn certificates() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
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
    let mut server_params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    server_params.use_authority_key_identifier_extension = true;
    let server = server_params.signed_by(&server_key, &ca, &ca_key).unwrap();
    (
        ca.pem().into_bytes(),
        server.pem().into_bytes(),
        server_key.serialize_pem().into_bytes(),
    )
}

#[derive(Debug, PartialEq, Eq)]
enum TlsMaterialError {
    MissingCertificate,
    InvalidCertificate,
    MissingPrivateKey,
    InvalidPrivateKey,
    Mismatched,
}

fn server_config(certificate_pem: &[u8], private_key_pem: &[u8]) -> Result<ServerConfig, TlsMaterialError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certificates = CertificateDer::pem_slice_iter(certificate_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| TlsMaterialError::InvalidCertificate)?;
    if certificates.is_empty() {
        return Err(TlsMaterialError::MissingCertificate);
    }
    let private_key = PrivateKeyDer::from_pem_slice(private_key_pem).map_err(|error| match error {
        rustls_pki_types::pem::Error::NoItemsFound => TlsMaterialError::MissingPrivateKey,
        _ => TlsMaterialError::InvalidPrivateKey,
    })?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(|_| TlsMaterialError::Mismatched)
}

enum InvalidTlsMaterial {
    MissingCertificate,
    MalformedCertificate,
    MissingPrivateKey,
    MalformedPrivateKey,
    Mismatched,
}

#[rstest]
#[case::missing_certificate(InvalidTlsMaterial::MissingCertificate, TlsMaterialError::MissingCertificate)]
#[case::malformed_certificate(InvalidTlsMaterial::MalformedCertificate, TlsMaterialError::InvalidCertificate)]
#[case::missing_private_key(InvalidTlsMaterial::MissingPrivateKey, TlsMaterialError::MissingPrivateKey)]
#[case::malformed_private_key(InvalidTlsMaterial::MalformedPrivateKey, TlsMaterialError::InvalidPrivateKey)]
#[case::mismatched(InvalidTlsMaterial::Mismatched, TlsMaterialError::Mismatched)]
fn test_starttls_rejects_invalid_material(#[case] material: InvalidTlsMaterial, #[case] expected: TlsMaterialError) {
    let (_, mut certificate, mut private_key) = certificates();
    match material {
        InvalidTlsMaterial::MissingCertificate => certificate.clear(),
        InvalidTlsMaterial::MalformedCertificate => {
            certificate = b"-----BEGIN CERTIFICATE-----\nnot-base64\n-----END CERTIFICATE-----\n".to_vec();
        }
        InvalidTlsMaterial::MissingPrivateKey => private_key.clear(),
        InvalidTlsMaterial::MalformedPrivateKey => {
            private_key = b"-----BEGIN PRIVATE KEY-----\nnot-base64\n-----END PRIVATE KEY-----\n".to_vec();
        }
        InvalidTlsMaterial::Mismatched => private_key = certificates().2,
    }

    assert_eq!(server_config(&certificate, &private_key).unwrap_err(), expected);
}

fn handle_connection(mut stream: TcpStream, config: Arc<ServerConfig>) {
    let message = read_message(&mut stream).expect("connection closed before StartTLS");
    let (message_id, operation, _) = parse_message(&message);
    assert_eq!(operation, 0x77, "expected StartTLS request");
    stream
        .write_all(&ldap_message(message_id, 0x78, ldap_result(0)))
        .unwrap();
    stream.flush().unwrap();
    let mut stream = StreamOwned::new(ServerConnection::new(config).unwrap(), stream);
    while let Some(message) = read_message(&mut stream) {
        let (message_id, operation, body) = parse_message(&message);
        if operation == 0x60 {
            handle_bind(&mut stream, message_id, body);
        } else {
            assert_eq!(operation, 0x63, "expected LDAP search request");
            handle_search(&mut stream, message_id, body);
        }
        stream.flush().unwrap();
    }
}

fn handle_bind(stream: &mut StreamOwned<ServerConnection, TcpStream>, message_id: u64, body: &[u8]) {
    let mut fields = BerReader::new(body);
    fields.expect(0x02);
    let dn = fields.string(0x04);
    let password = fields.string(0x80);
    let result = if dn.ends_with(",not-a-dn") {
        34
    } else if matches!(
        (dn.as_str(), password.as_str()),
        (ADMIN_DN, "GoodNewsEveryone") | (FRY_DN, "fry") | (BENDER_DN, "bender")
    ) {
        0
    } else {
        49
    };
    stream
        .write_all(&ldap_message(message_id, 0x61, ldap_result(result)))
        .unwrap();
}

fn handle_search(stream: &mut StreamOwned<ServerConnection, TcpStream>, message_id: u64, body: &[u8]) {
    let mut fields = BerReader::new(body);
    let base = fields.string(0x04);
    fields.expect(0x0a);
    fields.expect(0x0a);
    fields.expect(0x02);
    fields.expect(0x02);
    fields.expect(0x01);
    let (filter_tag, filter_body) = fields.element();
    let filter = search_filter(filter_tag, filter_body);
    let (_, attributes_body) = fields.element();
    let mut attribute_fields = BerReader::new(attributes_body);
    let mut attributes = Vec::new();
    while !attribute_fields.is_empty() {
        attributes.push(attribute_fields.string(0x04));
    }
    if base == "not-a-dn" {
        stream
            .write_all(&ldap_message(message_id, 0x65, ldap_result(34)))
            .unwrap();
        return;
    }
    let entries = directory_entries()
        .into_iter()
        .filter(|entry| entry.matches(&base, &filter))
        .collect::<Vec<_>>();
    for entry in &entries {
        stream
            .write_all(&ldap_message(message_id, 0x64, entry.encode(&attributes)))
            .unwrap();
    }
    stream
        .write_all(&ldap_message(message_id, 0x65, ldap_result(0)))
        .unwrap();
}

enum SearchFilter {
    Equality(String, String),
    Present(String),
}

fn search_filter(tag: u8, body: &[u8]) -> SearchFilter {
    if tag == 0x87 {
        return SearchFilter::Present(String::from_utf8(body.to_vec()).unwrap());
    }
    assert_eq!(tag, 0xa3, "expected an LDAP equality filter");
    let mut fields = BerReader::new(body);
    SearchFilter::Equality(fields.string(0x04), fields.string(0x04))
}

struct DirectoryEntry {
    dn: &'static str,
    uid: &'static str,
    subject: &'static str,
    display_name: &'static str,
}

impl DirectoryEntry {
    fn matches(&self, base: &str, filter: &SearchFilter) -> bool {
        if !self.dn.eq_ignore_ascii_case(base) && !base.eq_ignore_ascii_case(PEOPLE_DN) {
            return false;
        }
        match filter {
            SearchFilter::Equality(attribute, value) => {
                if attribute.eq_ignore_ascii_case("uid") {
                    self.uid == value
                } else {
                    assert!(attribute.eq_ignore_ascii_case("objectClass"));
                    value == "inetOrgPerson"
                }
            }
            SearchFilter::Present(attribute) => attribute.eq_ignore_ascii_case("objectClass"),
        }
    }

    fn encode(&self, requested: &[String]) -> Vec<u8> {
        let attributes = requested
            .iter()
            .filter_map(|name| self.attribute(name).map(|values| partial_attribute(name, values)))
            .collect::<Vec<_>>();
        [octet(self.dn), tlv(0x30, concatenate(attributes))].concat()
    }

    fn attribute(&self, name: &str) -> Option<Vec<String>> {
        if name.eq_ignore_ascii_case("entryUUID") {
            return Some(vec![self.subject.to_owned()]);
        }
        if name.eq_ignore_ascii_case("displayName") {
            return Some(vec![self.display_name.to_owned()]);
        }
        if name.eq_ignore_ascii_case("memberOf") {
            return Some(vec!["cn=ship_crew,ou=groups,dc=localhost".to_owned()]);
        }
        if name.eq_ignore_ascii_case("title") {
            return Some(vec!["x".repeat(1_025)]);
        }
        if name.eq_ignore_ascii_case("description") {
            let mut descriptions = String::new();
            for value in 0..=MAX_EXTERNAL_GROUPS {
                writeln!(descriptions, "group-{value}").unwrap();
            }
            return Some(descriptions.lines().map(str::to_owned).collect());
        }
        None
    }
}

const fn directory_entries() -> [DirectoryEntry; 2] {
    [
        DirectoryEntry {
            dn: FRY_DN,
            uid: "fry",
            subject: "fry-id",
            display_name: "Fry",
        },
        DirectoryEntry {
            dn: BENDER_DN,
            uid: "bender",
            subject: "bender-id",
            display_name: "Bender",
        },
    ]
}

fn partial_attribute(name: &str, values: Vec<String>) -> Vec<u8> {
    tlv(
        0x30,
        [
            octet(name),
            tlv(0x31, concatenate(values.into_iter().map(octet).collect())),
        ]
        .concat(),
    )
}

fn ldap_message(message_id: u64, operation: u8, body: Vec<u8>) -> Vec<u8> {
    tlv(0x30, [integer(message_id), tlv(operation, body)].concat())
}

fn ldap_result(code: u8) -> Vec<u8> {
    [tlv(0x0a, vec![code]), octet(""), octet("")].concat()
}

fn integer(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|byte| *byte != 0).unwrap_or(bytes.len() - 1);
    let body = std::iter::repeat_n(0, usize::from(bytes[first] & 0x80 != 0))
        .chain(bytes[first..].iter().copied())
        .collect();
    tlv(0x02, body)
}

fn octet(value: impl AsRef<[u8]>) -> Vec<u8> {
    tlv(0x04, value.as_ref().to_vec())
}

fn concatenate(values: Vec<Vec<u8>>) -> Vec<u8> {
    values.into_iter().flatten().collect()
}

fn tlv(tag: u8, body: Vec<u8>) -> Vec<u8> {
    let mut encoded = vec![tag];
    encode_length(body.len(), &mut encoded);
    encoded.extend(body);
    encoded
}

fn encode_length(length: usize, encoded: &mut Vec<u8>) {
    if length < 128 {
        encoded.push(u8::try_from(length).unwrap());
        return;
    }
    let bytes = length.to_be_bytes();
    let first = bytes.iter().position(|byte| *byte != 0).unwrap();
    encoded.push(0x80 | u8::try_from(bytes.len() - first).unwrap());
    encoded.extend_from_slice(&bytes[first..]);
}

fn read_message(reader: &mut impl std::io::Read) -> Option<Vec<u8>> {
    let mut tag = [0];
    if reader.read_exact(&mut tag).is_err() {
        return None;
    }
    assert_eq!(tag[0], 0x30, "expected an LDAP message");
    let length = read_length(reader);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Some(body)
}

fn read_length(reader: &mut impl std::io::Read) -> usize {
    let mut first = [0];
    reader.read_exact(&mut first).unwrap();
    assert_eq!(first[0] & 0x80, 0, "test requests must use short BER lengths");
    first[0] as usize
}

fn parse_message(message: &[u8]) -> (u64, u8, &[u8]) {
    let mut fields = BerReader::new(message);
    let (_, message_id) = fields.element();
    let message_id = message_id.iter().fold(0, |value, byte| (value << 8) | u64::from(*byte));
    let (operation, body) = fields.element();
    (message_id, operation, body)
}

struct BerReader<'a> {
    body: &'a [u8],
    offset: usize,
}

impl<'a> BerReader<'a> {
    const fn new(body: &'a [u8]) -> Self {
        Self { body, offset: 0 }
    }

    const fn is_empty(&self) -> bool {
        self.offset == self.body.len()
    }

    fn expect(&mut self, expected: u8) -> &'a [u8] {
        let (tag, body) = self.element();
        assert_eq!(tag, expected, "unexpected BER tag");
        body
    }

    fn string(&mut self, expected: u8) -> String {
        String::from_utf8(self.expect(expected).to_vec()).unwrap()
    }

    fn element(&mut self) -> (u8, &'a [u8]) {
        let tag = self.body[self.offset];
        self.offset += 1;
        let first = self.body[self.offset];
        self.offset += 1;
        assert_eq!(first & 0x80, 0, "test request fields must use short BER lengths");
        let length = first as usize;
        let end = self.offset + length;
        let body = &self.body[self.offset..end];
        self.offset = end;
        (tag, body)
    }
}
