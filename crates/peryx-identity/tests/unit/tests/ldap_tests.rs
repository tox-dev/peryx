use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rstest::rstest;
use url::Url;

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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let disconnected = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        socket.set_zero_linger().unwrap();
        drop(socket);
    });
    let mut settings = settings();
    settings.url = Url::parse(&format!("ldap://127.0.0.1:{port}")).unwrap();
    settings.request_timeout = Duration::from_secs(1);
    let provider = LdapProvider::new(settings).unwrap();

    assert_eq!(
        provider.authenticate("alice", "secret").await.unwrap_err(),
        LdapProviderError::Unavailable
    );
    disconnected.await.unwrap();
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
