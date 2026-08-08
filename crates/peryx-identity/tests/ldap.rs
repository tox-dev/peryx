use std::fmt::Write as _;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use peryx_identity::{
    ExternalGroupGrant, ExternalIdentityResolution, ExternalIdentityStore, ExternalLinkRequest, ExternalLogin,
    GrantScope, LdapBindMode, LdapLoginError, LdapLoginService, LdapProvider, LdapProviderError, LdapProviderSettings,
    MAX_EXTERNAL_GROUPS, ProviderId, Role, ServerUser, UserId, UserName, UserState,
};
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
use testcontainers::core::{CmdWaitFor, ExecCommand, ImageExt as _, IntoContainerPort as _};
use testcontainers::runners::AsyncRunner as _;
use testcontainers::{ContainerAsync, GenericImage};
use url::Url;

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
    let fry = stable_authenticate(&search_provider, "fry", "fry")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(fry.display_name.display(), "Fry");
    assert!(
        fry.groups
            .iter()
            .any(|group| group.as_str().starts_with("cn=ship_crew,"))
    );
    assert_eq!(
        stable_authenticate(&search_provider, "fry", "wrong").await.unwrap(),
        None
    );
    assert_eq!(
        stable_authenticate(&search_provider, "fry)(uid=*)", "fry")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        stable_authenticate(&search_provider, "bender", "bender")
            .await
            .unwrap()
            .unwrap()
            .display_name
            .display(),
        "Bender"
    );
    assert_eq!(
        stable_authenticate(&direct_provider, "Philip J. Fry", "wrong")
            .await
            .unwrap(),
        None
    );
    let direct = stable_authenticate(&direct_provider, "Philip J. Fry", "fry")
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
    // Coverage can slow Docker enough to reject a start transiently.
    let container = tokio::time::timeout(Duration::from_mins(3), async {
        loop {
            let started = GenericImage::new("ghcr.io/rroemhild/docker-test-openldap", "v2.5.0")
                .with_exposed_port(10_389.tcp())
                .with_hostname("localhost")
                .with_env_var("LDAP_DOMAIN", "localhost")
                .with_env_var("LDAP_BASEDN", "dc=localhost")
                .with_env_var("LDAP_BINDDN", "cn=admin,dc=localhost")
                .with_copy_to("/etc/ldap/ssl/ca.crt", ca.clone())
                .with_copy_to("/etc/ldap/ssl/ldap.crt", certificate.clone())
                .with_copy_to("/etc/ldap/ssl/ldap.key", key.clone())
                .with_copy_to("/tmp/invalid-attributes.ldif", invalid_attributes.clone().into_bytes())
                .start()
                .await;
            match started {
                Ok(container) => break container,
                Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    })
    .await
    .expect("openldap container never started within the retry window");
    let port = container.get_host_port_ipv4(10_389.tcp()).await.unwrap();
    wait_until_serving(port, &ca).await;
    (container, port, ca)
}

// The image health check does not prove that the published host port accepts StartTLS.
async fn wait_until_serving(port: u16, ca: &[u8]) {
    let probe = LdapProvider::new(openldap_settings(port, ca.to_vec(), search_bind())).unwrap();
    tokio::time::timeout(Duration::from_mins(2), async {
        while !matches!(probe.authenticate("fry", "fry").await, Ok(Some(_))) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("openldap never began serving StartTLS binds");
}

// A lazy pool can race the container's first StartTLS request under load.
const STABLE_TIMEOUT: Duration = Duration::from_secs(30);

async fn stable_authenticate(
    provider: &LdapProvider,
    username: &str,
    password: &str,
) -> Result<Option<ExternalLogin>, LdapProviderError> {
    tokio::time::timeout(STABLE_TIMEOUT, async {
        loop {
            match provider.authenticate(username, password).await {
                Err(LdapProviderError::Unavailable | LdapProviderError::Timeout) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                stable => return stable,
            }
        }
    })
    .await
    .expect("LDAP authenticate never returned a stable result")
}

async fn stable_service_authenticate<S: ExternalIdentityStore + Sync>(
    service: &LdapLoginService<S>,
    username: &str,
    password: &str,
) -> Result<Option<ExternalIdentityResolution>, LdapLoginError<S::Error>> {
    tokio::time::timeout(STABLE_TIMEOUT, async {
        loop {
            match service.authenticate(username, password).await {
                Err(LdapLoginError::Provider(LdapProviderError::Unavailable | LdapProviderError::Timeout)) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                stable => return stable,
            }
        }
    })
    .await
    .expect("LDAP login never returned a stable result")
}

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

    assert_eq!(
        stable_service_authenticate(&service, "fry", "wrong").await.unwrap(),
        None
    );
    assert_eq!(requests.lock().unwrap().len(), 0);
    let resolution = stable_service_authenticate(&service, "fry", "fry")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolution.user.id, stable_user);
    let request = requests.lock().unwrap().pop().unwrap();
    assert_eq!(request.identity.subject, fry.identity.subject);
    assert_eq!(request.grants.len(), 1);
    assert_eq!(request.grants[0].role, Role::RepositoryReader);
}

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
        let invalid = LdapProvider::new(invalid).unwrap();
        assert_eq!(
            stable_authenticate(&invalid, "fry", "fry").await.unwrap_err(),
            LdapProviderError::InvalidEntry
        );
    }
    let mut no_groups = openldap_settings(port, certificate.to_vec(), search_bind());
    no_groups.group_attribute = None;
    let no_groups = LdapProvider::new(no_groups).unwrap();
    assert!(
        stable_authenticate(&no_groups, "fry", "fry")
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
            bind_dn: "cn=admin,dc=localhost".to_owned(),
            bind_password: "GoodNewsEveryone".to_owned(),
        },
    );
    let ambiguous = LdapProvider::new(ambiguous).unwrap();
    assert_eq!(
        stable_authenticate(&ambiguous, "inetOrgPerson", "irrelevant")
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
    let failing_service = LdapLoginService::new(
        LdapProvider::new(openldap_settings(port, certificate, search_bind())).unwrap(),
        |_request: ExternalLinkRequest| -> Result<ExternalIdentityResolution, &'static str> { Err("write failed") },
        Vec::new(),
    );
    assert_eq!(
        stable_service_authenticate(&failing_service, "fry", "fry")
            .await
            .unwrap_err(),
        LdapLoginError::Store("write failed")
    );
}

fn search_bind() -> LdapBindMode {
    LdapBindMode::Search {
        username_attribute: "uid".to_owned(),
        bind_dn: "cn=admin,dc=localhost".to_owned(),
        bind_password: "GoodNewsEveryone".to_owned(),
    }
}

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
