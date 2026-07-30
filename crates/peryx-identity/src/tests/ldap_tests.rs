use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::fmt::Write as _;

#[cfg(target_os = "linux")]
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
use rstest::rstest;
#[cfg(target_os = "linux")]
use testcontainers::core::{CmdWaitFor, ExecCommand, ImageExt as _, IntoContainerPort as _, WaitFor};
#[cfg(target_os = "linux")]
use testcontainers::runners::AsyncRunner as _;
#[cfg(target_os = "linux")]
use testcontainers::{ContainerAsync, GenericImage};
use url::Url;

#[cfg(target_os = "linux")]
use crate::{ExternalGroupGrant, ExternalLogin, GrantScope, MAX_EXTERNAL_GROUPS, Role};
use crate::{
    ExternalIdentityResolution, ExternalLinkRequest, LdapBindMode, LdapLoginError, LdapLoginService, LdapProvider,
    LdapProviderBuildError, LdapProviderError, LdapProviderSettings, ProviderId, ServerUser, UserId, UserName,
    UserState,
};

fn settings() -> LdapProviderSettings {
    LdapProviderSettings {
        id: ProviderId::new("corporate").unwrap(),
        url: Url::parse("ldap://127.0.0.1:9").unwrap(),
        base_dn: "ou=people,dc=example,dc=com".to_owned(),
        bind: LdapBindMode::Direct {
            dn_attribute: "uid".to_owned(),
        },
        subject_attribute: "entryUUID".to_owned(),
        display_name_attribute: "displayName".to_owned(),
        group_attribute: Some("memberOf".to_owned()),
        custom_ca_pem: None,
        connect_timeout: Duration::from_millis(20),
        request_timeout: Duration::from_millis(40),
        max_connections: NonZeroU32::new(2).unwrap(),
    }
}

#[test]
fn test_ldap_settings_and_provider_debug_redact_bind_password() {
    let mut settings = settings();
    assert_eq!(format!("{:?}", settings.bind), "Direct { dn_attribute: \"uid\" }");
    settings.bind = LdapBindMode::Search {
        username_attribute: "uid".to_owned(),
        bind_dn: "cn=service,dc=example,dc=com".to_owned(),
        bind_password: "directory-secret".to_owned(),
    };

    let settings_debug = format!("{settings:?}");
    let provider_debug = format!("{:?}", LdapProvider::new(settings).unwrap());

    assert!(settings_debug.contains("[redacted]"));
    assert!(provider_debug.contains("[redacted]"));
    assert!(!settings_debug.contains("directory-secret"));
    assert!(!provider_debug.contains("directory-secret"));
}

#[rstest]
#[case::https("https://localhost", LdapProviderBuildError::InvalidUrl)]
#[case::missing_host("ldap:///", LdapProviderBuildError::InvalidUrl)]
#[case::username("ldap://user@localhost", LdapProviderBuildError::InvalidUrl)]
#[case::password("ldap://user:secret@localhost", LdapProviderBuildError::InvalidUrl)]
#[case::path("ldap://localhost/users", LdapProviderBuildError::InvalidUrl)]
#[case::query("ldap://localhost/?scope=sub", LdapProviderBuildError::InvalidUrl)]
#[case::fragment("ldap://localhost/#users", LdapProviderBuildError::InvalidUrl)]
fn test_ldap_provider_rejects_unsafe_urls(#[case] url: &str, #[case] expected: LdapProviderBuildError) {
    let mut settings = settings();
    settings.url = Url::parse(url).unwrap();

    assert_eq!(LdapProvider::new(settings).unwrap_err(), expected);
}

#[rstest]
#[case::empty_base("", None, LdapProviderBuildError::InvalidDn)]
#[case::long_base(&"a".repeat(4_097), None, LdapProviderBuildError::InvalidDn)]
#[case::empty_subject("dc=example", Some(("", "displayName")), LdapProviderBuildError::InvalidAttribute)]
#[case::bad_display("dc=example", Some(("entryUUID", "display_name")), LdapProviderBuildError::InvalidAttribute)]
fn test_ldap_provider_rejects_invalid_names(
    #[case] base_dn: &str,
    #[case] attributes: Option<(&str, &str)>,
    #[case] expected: LdapProviderBuildError,
) {
    let mut settings = settings();
    settings.base_dn = base_dn.to_owned();
    if let Some((subject, display)) = attributes {
        settings.subject_attribute = subject.to_owned();
        settings.display_name_attribute = display.to_owned();
    }

    assert_eq!(LdapProvider::new(settings).unwrap_err(), expected);
}

#[test]
fn test_ldap_provider_rejects_a_long_attribute() {
    let mut settings = settings();
    settings.subject_attribute = "a".repeat(129);

    assert_eq!(
        LdapProvider::new(settings).unwrap_err(),
        LdapProviderBuildError::InvalidAttribute
    );
}

#[rstest]
#[case::direct_attribute(LdapBindMode::Direct { dn_attribute: "uid_name".to_owned() }, LdapProviderBuildError::InvalidAttribute)]
#[case::search_attribute(
    LdapBindMode::Search {
        username_attribute: "uid_name".to_owned(),
        bind_dn: "cn=service,dc=example".to_owned(),
        bind_password: "secret".to_owned(),
    },
    LdapProviderBuildError::InvalidAttribute
)]
#[case::search_dn(
    LdapBindMode::Search {
        username_attribute: "uid".to_owned(),
        bind_dn: String::new(),
        bind_password: "secret".to_owned(),
    },
    LdapProviderBuildError::InvalidDn
)]
#[case::search_password(
    LdapBindMode::Search {
        username_attribute: "uid".to_owned(),
        bind_dn: "cn=service,dc=example".to_owned(),
        bind_password: String::new(),
    },
    LdapProviderBuildError::EmptyBindPassword
)]
fn test_ldap_provider_rejects_invalid_bind_settings(
    #[case] bind: LdapBindMode,
    #[case] expected: LdapProviderBuildError,
) {
    let mut settings = settings();
    settings.bind = bind;

    assert_eq!(LdapProvider::new(settings).unwrap_err(), expected);
}

#[rstest]
#[case::connect(true)]
#[case::request(false)]
fn test_ldap_provider_rejects_zero_timeouts(#[case] connect: bool) {
    let mut settings = settings();
    if connect {
        settings.connect_timeout = Duration::ZERO;
    } else {
        settings.request_timeout = Duration::ZERO;
    }

    assert_eq!(
        LdapProvider::new(settings).unwrap_err(),
        LdapProviderBuildError::InvalidTimeout
    );
}

#[rstest]
#[case::empty(b"", LdapProviderBuildError::EmptyCa)]
#[case::malformed(
    b"-----BEGIN CERTIFICATE-----\nnot-base64\n-----END CERTIFICATE-----\n",
    LdapProviderBuildError::InvalidCa
)]
#[case::invalid_der(
    b"-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n",
    LdapProviderBuildError::InvalidCa
)]
fn test_ldap_provider_rejects_invalid_custom_ca(#[case] pem: &[u8], #[case] expected: LdapProviderBuildError) {
    let mut settings = settings();
    settings.custom_ca_pem = Some(pem.to_vec());

    assert_eq!(LdapProvider::new(settings).unwrap_err(), expected);
}

#[tokio::test]
async fn test_ldap_provider_rejects_empty_credentials_without_connecting() {
    let provider = LdapProvider::new(settings()).unwrap();

    assert_eq!(provider.id().as_str(), "corporate");
    assert_eq!(provider.authenticate("", "secret").await.unwrap(), None);
    assert_eq!(provider.authenticate("alice", "").await.unwrap(), None);
}

#[tokio::test]
async fn test_ldap_provider_reports_an_unavailable_directory() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut settings = settings();
    settings.url = Url::parse(&format!("ldap://127.0.0.1:{port}")).unwrap();
    settings.connect_timeout = Duration::from_secs(1);
    settings.request_timeout = Duration::from_secs(1);
    let provider = LdapProvider::new(settings).unwrap();

    assert_eq!(
        provider.authenticate("alice", "secret").await.unwrap_err(),
        LdapProviderError::Unavailable
    );
}

#[tokio::test]
async fn test_ldap_provider_reports_a_request_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stalled = tokio::spawn(async move {
        let _socket = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let mut settings = settings();
    settings.url = Url::parse(&format!("ldap://127.0.0.1:{port}")).unwrap();
    settings.connect_timeout = Duration::from_secs(1);
    settings.request_timeout = Duration::from_millis(20);
    let provider = LdapProvider::new(settings).unwrap();

    assert_eq!(
        provider.authenticate("alice", "secret").await.unwrap_err(),
        LdapProviderError::Timeout
    );
    stalled.abort();
}

#[tokio::test]
async fn test_ldap_login_service_exposes_identity_and_store_errors() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut service_settings = settings();
    service_settings.url = Url::parse(&format!("ldap://127.0.0.1:{port}")).unwrap();
    let service = LdapLoginService::new(
        LdapProvider::new(service_settings).unwrap(),
        move |request: ExternalLinkRequest| -> Result<ExternalIdentityResolution, &'static str> {
            captured.lock().unwrap().push(request);
            Ok(ExternalIdentityResolution {
                user: ServerUser {
                    id: UserId::random(),
                    name: UserName::new("Alice").unwrap(),
                    state: UserState::Active,
                    revision: 1,
                },
                link_created: true,
                grants_changed: false,
            })
        },
        Vec::new(),
    );

    assert_eq!(service.id().as_str(), "corporate");
    assert!(format!("{service:?}").contains("corporate"));
    assert_eq!(service.authenticate("", "secret").await.unwrap(), None);
    assert_eq!(
        service.authenticate("alice", "secret").await.unwrap_err(),
        LdapLoginError::Provider(LdapProviderError::Unavailable)
    );
    assert_eq!(requests.lock().unwrap().len(), 0);
    assert_eq!(
        LdapLoginError::<&str>::Provider(LdapProviderError::Timeout).to_string(),
        "LDAP provider failed: LDAP request timed out"
    );
    assert_eq!(
        LdapLoginError::Store("disk unavailable").to_string(),
        "external identity store failed: disk unavailable"
    );
}

#[test]
fn test_ldap_numeric_oid_attributes_are_accepted() {
    let mut settings = settings();
    settings.subject_attribute = "1.3.6.1.1.16.4".to_owned();
    settings.group_attribute = None;

    assert!(LdapProvider::new(settings).is_ok());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_ldap_login_crosses_starttls_bind_and_store_boundaries() {
    let (container, port, certificate) = start_openldap().await;
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

    assert_service_link(search_provider, &fry).await;
    install_invalid_attributes(&container).await;
    assert_invalid_entries(port, &certificate).await;
    assert_directory_errors(port, &certificate).await;
    assert_store_error(port, certificate).await;
}

#[cfg(target_os = "linux")]
async fn start_openldap() -> (ContainerAsync<GenericImage>, u16, Vec<u8>) {
    let (ca, certificate, key) = openldap_certificates();
    let mut descriptions = String::new();
    for value in 0..=MAX_EXTERNAL_GROUPS {
        writeln!(descriptions, "description: group-{value}").unwrap();
    }
    let invalid_attributes = format!(
        "dn: cn=Philip J. Fry,ou=people,dc=localhost\nchangetype: modify\nreplace: title\ntitle: {}\n-\nreplace: description\n{descriptions}",
        "x".repeat(1_025)
    );
    let container = GenericImage::new("ghcr.io/rroemhild/docker-test-openldap", "v2.5.0")
        .with_exposed_port(10_389.tcp())
        .with_wait_for(WaitFor::message_on_stderr("slapd starting"))
        .with_env_var("LDAP_DOMAIN", "localhost")
        .with_env_var("LDAP_BASEDN", "dc=localhost")
        .with_env_var("LDAP_BINDDN", "cn=admin,dc=localhost")
        .with_copy_to("/etc/ldap/ssl/ldap.crt", certificate)
        .with_copy_to("/etc/ldap/ssl/ldap.key", key)
        .with_copy_to("/tmp/invalid-attributes.ldif", invalid_attributes.into_bytes())
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(10_389.tcp()).await.unwrap();
    (container, port, ca)
}

#[cfg(target_os = "linux")]
fn openldap_certificates() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
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

#[cfg(target_os = "linux")]
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
                name: "packages".to_owned(),
            },
        }],
    );

    assert_eq!(service.authenticate("fry", "wrong").await.unwrap(), None);
    assert_eq!(requests.lock().unwrap().len(), 0);
    let resolution = service.authenticate("fry", "fry").await.unwrap().unwrap();
    assert_eq!(resolution.user.id, stable_user);
    let request = requests.lock().unwrap().pop().unwrap();
    assert_eq!(request.identity.subject, fry.identity.subject);
    assert_eq!(request.grants.len(), 1);
    assert_eq!(request.grants[0].role, Role::RepositoryReader);
}

#[cfg(target_os = "linux")]
async fn install_invalid_attributes(container: &ContainerAsync<GenericImage>) {
    container
        .exec(
            ExecCommand::new([
                "ldapmodify",
                "-x",
                "-H",
                "ldap://localhost:10389",
                "-D",
                "cn=admin,dc=localhost",
                "-w",
                "GoodNewsEveryone",
                "-f",
                "/tmp/invalid-attributes.ldif",
            ])
            .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )
        .await
        .unwrap();
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
async fn assert_directory_errors(port: u16, certificate: &[u8]) {
    let ambiguous = openldap_settings(
        port,
        certificate.to_vec(),
        LdapBindMode::Search {
            username_attribute: "objectClass".to_owned(),
            bind_dn: "cn=admin,dc=localhost".to_owned(),
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
    invalid_search.base_dn = "not-a-dn".to_owned();
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
        bind_dn: "cn=admin,dc=localhost".to_owned(),
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
    invalid_direct.base_dn = "not-a-dn".to_owned();
    assert_eq!(
        LdapProvider::new(invalid_direct)
            .unwrap()
            .authenticate("fry", "fry")
            .await
            .unwrap_err(),
        LdapProviderError::Unavailable
    );
}

#[cfg(target_os = "linux")]
async fn assert_store_error(port: u16, certificate: Vec<u8>) {
    let failing_service = LdapLoginService::new(
        LdapProvider::new(openldap_settings(port, certificate, search_bind())).unwrap(),
        |_request: ExternalLinkRequest| -> Result<ExternalIdentityResolution, &'static str> { Err("write failed") },
        Vec::new(),
    );
    assert_eq!(
        failing_service.authenticate("fry", "fry").await.unwrap_err(),
        LdapLoginError::Store("write failed")
    );
}

#[cfg(target_os = "linux")]
fn search_bind() -> LdapBindMode {
    LdapBindMode::Search {
        username_attribute: "uid".to_owned(),
        bind_dn: "cn=admin,dc=localhost".to_owned(),
        bind_password: "GoodNewsEveryone".to_owned(),
    }
}

#[cfg(target_os = "linux")]
fn openldap_settings(port: u16, certificate: Vec<u8>, bind: LdapBindMode) -> LdapProviderSettings {
    LdapProviderSettings {
        id: ProviderId::new("openldap").unwrap(),
        url: Url::parse(&format!("ldap://localhost:{port}")).unwrap(),
        base_dn: "ou=people,dc=localhost".to_owned(),
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
