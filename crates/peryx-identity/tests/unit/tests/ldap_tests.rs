use std::num::NonZeroU32;
use std::time::Duration;

use rcgen::{CertificateParams, KeyPair};
use rstest::rstest;
use url::Url;

use crate::{
    LdapBindMode, LdapLoginError, LdapLoginService, LdapProvider, LdapProviderBuildError, LdapProviderError,
    LdapProviderSettings, ProviderId,
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
fn test_ldap_direct_bind_debug_reports_the_dn_attribute() {
    assert_eq!(format!("{:?}", settings().bind), "Direct { dn_attribute: \"uid\" }");
}

#[test]
fn test_ldap_settings_and_provider_debug_redact_bind_password() {
    let mut settings = settings();
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

#[test]
fn test_ldap_login_service_debug_reports_provider_and_mapping_count() {
    let provider = LdapProvider::new(settings()).unwrap();
    let expected = format!("LdapLoginService {{ provider: {provider:?}, group_mappings: 0, .. }}");

    assert_eq!(
        format!(
            "{:?}",
            LdapLoginService::new(
                provider,
                Result::<crate::ExternalIdentityResolution, crate::ExternalLinkRequest>::Err,
                Vec::new()
            )
        ),
        expected
    );
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

#[test]
fn test_ldap_provider_accepts_and_redacts_a_valid_custom_ca() {
    let certificate = CertificateParams::default()
        .self_signed(&KeyPair::generate().unwrap())
        .unwrap()
        .pem();
    let mut settings = settings();
    settings.custom_ca_pem = Some(certificate.as_bytes().to_vec());

    let debug = format!("{:?}", LdapProvider::new(settings).unwrap());

    assert!(debug.contains("custom_ca: true"));
    assert!(!debug.contains(&certificate));
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
    let (accepted, ready) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let stalled = tokio::spawn(async move {
        let _socket = listener.accept().await.unwrap();
        accepted.send(()).unwrap();
        released.await.unwrap();
    });
    let mut settings = settings();
    settings.url = Url::parse(&format!("ldap://127.0.0.1:{port}")).unwrap();
    settings.connect_timeout = Duration::from_secs(1);
    settings.request_timeout = Duration::from_millis(20);
    let provider = LdapProvider::new(settings).unwrap();
    let authentication = tokio::spawn(async move { provider.authenticate("alice", "secret").await });
    ready.await.unwrap();

    assert_eq!(authentication.await.unwrap().unwrap_err(), LdapProviderError::Timeout);
    release.send(()).unwrap();
    stalled.await.unwrap();
}

#[test]
fn test_ldap_login_errors_preserve_the_boundary() {
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
