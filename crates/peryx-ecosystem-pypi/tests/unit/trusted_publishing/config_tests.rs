use peryx_driver::serving::PluginIndexConfig;
use peryx_driver::state::{AppState, Index, IndexKind};
use peryx_identity::{IndexAcl, Signer};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use super::*;

fn values(source: &str) -> toml::Table {
    toml::from_str(source).unwrap()
}

fn publisher() -> toml::Table {
    values(
        r#"
oidc_audience = "peryx"

[[trusted_publisher]]
id = "release"
issuer = "https://issuer.example"
repository = "hosted"
subject = "repo:org/app:*"
projects = ["app"]
"#,
    )
}

fn config<'a>(values: &'a toml::Table, indexes: &'a [PluginIndexConfig<'a>]) -> PluginAuthConfig<'a> {
    PluginAuthConfig {
        values,
        signing_key_configured: true,
        token_ttl_secs: 300,
        indexes,
    }
}

#[test]
fn test_defaults_preserve_the_audience() {
    assert_eq!(auth_defaults()["oidc_audience"].as_str(), Some("peryx"));
}

#[test]
fn test_validate_accepts_unconfigured_auth() {
    let values = auth_defaults();
    let mut config = config(&values, &[]);
    config.signing_key_configured = false;

    assert_eq!(validate(config), Ok(()));
}

#[test]
fn test_validate_accepts_a_writable_pypi_repository() {
    let values = publisher();
    let indexes = [PluginIndexConfig {
        name: "hosted",
        ecosystem: crate::ECOSYSTEM,
        writable: true,
    }];
    assert_eq!(validate(config(&values, &indexes)), Ok(()));
}

#[test]
fn test_validate_requires_a_signing_key() {
    let values = publisher();
    let indexes = [PluginIndexConfig {
        name: "hosted",
        ecosystem: crate::ECOSYSTEM,
        writable: true,
    }];
    let mut config = config(&values, &indexes);
    config.signing_key_configured = false;
    assert_eq!(
        validate(config),
        Err("auth: `signing_key` is required when trusted publishers are configured".to_owned())
    );
}

#[test]
fn test_validate_rejects_duplicate_publisher_ids() {
    let mut values = publisher();
    let duplicate = values["trusted_publisher"].as_array().unwrap()[0].clone();
    values["trusted_publisher"].as_array_mut().unwrap().push(duplicate);
    let indexes = [PluginIndexConfig {
        name: "hosted",
        ecosystem: crate::ECOSYSTEM,
        writable: true,
    }];

    assert_eq!(
        validate(config(&values, &indexes)),
        Err("trusted publisher release: publisher IDs must be unique".to_owned())
    );
}

#[test]
fn test_validate_rejects_a_repository_without_a_writable_pypi_index() {
    let values = publisher();
    assert_eq!(
        validate(config(&values, &[])),
        Err(
            "trusted publisher release: repository must name a writable index with trusted publishing support"
                .to_owned()
        )
    );
}

#[test]
fn test_validate_rejects_empty_owned_fields() {
    let values = values(
        r#"
oidc_audience = ""
trusted_publisher = []
"#,
    );
    assert_eq!(
        validate(config(&values, &[])),
        Err("auth: `oidc_audience` must not be empty".to_owned())
    );
}

#[test]
fn test_validate_rejects_an_empty_trusted_endpoint_host() {
    let values = values(
        r#"
oidc_trusted_endpoint_hosts = [" "]
trusted_publisher = []
"#,
    );
    assert_eq!(
        validate(config(&values, &[])),
        Err("auth: `oidc_trusted_endpoint_hosts` entries must not be empty".to_owned())
    );
}

/// An issuer whose key endpoint sits on a second internal host needs that host approved, so the
/// list has to reach the runtime rather than stop at parsing.
#[test]
fn test_install_accepts_approved_trusted_endpoint_hosts() {
    let (_dir, mut state) = state("private");
    state.set_token_realm(Signer::new(b"key", "peryx"), 300).unwrap();
    let mut values = publisher();
    values.insert(
        "oidc_trusted_endpoint_hosts".to_owned(),
        toml::Value::Array(vec![toml::Value::String("keys.corp.internal".to_owned())]),
    );

    install_auth(&mut state, &values).unwrap();

    assert!(crate::trusted_publishing_enabled(&state));
}

/// The outbound policy trusts each configured issuer host, so an issuer it cannot read a host from
/// fails installation rather than widening the policy.
#[test]
fn test_install_rejects_an_issuer_without_a_host() {
    let (_dir, mut state) = state("private");
    state.set_token_realm(Signer::new(b"key", "peryx"), 300).unwrap();
    let mut values = publisher();
    let publishers = values.get_mut("trusted_publisher").unwrap().as_array_mut().unwrap();
    publishers[0].as_table_mut().unwrap()["issuer"] = toml::Value::String("issuer.example".to_owned());

    assert_eq!(
        install_auth(&mut state, &values),
        Err("trusted publishing is misconfigured".to_owned())
    );
}

#[test]
fn test_validate_rejects_empty_publisher_fields() {
    let values = values(
        r#"
[[trusted_publisher]]
id = ""
issuer = "https://issuer.example"
repository = "hosted"
subject = "*"
projects = ["app"]
"#,
    );
    assert_eq!(
        validate(config(&values, &[])),
        Err("auth: trusted publisher fields and project lists must not be empty".to_owned())
    );
}

#[test]
fn test_validate_prefixes_deserialization_errors() {
    let values = values("oidc_audience = 1");
    let parse_error = toml::Value::Table(values.clone()).try_into::<Config>().err().unwrap();

    assert_eq!(validate(config(&values, &[])), Err(format!("auth: {parse_error}")));
}

#[test]
fn test_unconfigured_auth_allocates_no_runtime_service() {
    let (_dir, mut state) = state("private");

    install_auth(&mut state, &auth_defaults()).unwrap();

    assert!(!crate::trusted_publishing_enabled(&state));
    assert_eq!(state.http_routes().count(), 0);
}

#[test]
fn test_install_requires_the_validated_signing_key() {
    let (_dir, mut state) = state("private");

    assert_eq!(
        install_auth(&mut state, &publisher()),
        Err("auth: `signing_key` is required when trusted publishers are configured".to_owned())
    );
}

#[test]
fn test_install_registers_runtime_and_routes() {
    let (_dir, mut state) = state("private");
    state.set_token_realm(Signer::new(b"key", "peryx"), 300).unwrap();

    install_auth(&mut state, &publisher()).unwrap();

    assert!(crate::trusted_publishing_enabled(&state));
    assert_eq!(state.http_routes().count(), 1);
}

#[test]
fn test_install_accepts_a_virtual_write_route() {
    let (_dir, mut state) = state("private");
    state.set_token_realm(Signer::new(b"key", "peryx"), 300).unwrap();
    let mut values = publisher();
    values["trusted_publisher"].as_array_mut().unwrap()[0]["repository"] = toml::Value::String("root-pypi".to_owned());

    install_auth(&mut state, &values).unwrap();

    assert!(crate::trusted_publishing_enabled(&state));
}

#[rstest]
#[case::unknown("missing")]
#[case::read_only("read-only")]
fn test_install_rejects_an_unwritable_repository(#[case] repository: &str) {
    let (_dir, mut state) = state("private");
    state.set_token_realm(Signer::new(b"key", "peryx"), 300).unwrap();
    let mut values = publisher();
    values["trusted_publisher"].as_array_mut().unwrap()[0]["repository"] = toml::Value::String(repository.to_owned());

    assert_eq!(
        install_auth(&mut state, &values),
        Err(
            "trusted publisher release: repository must name a writable index with trusted publishing support"
                .to_owned()
        )
    );
}

#[test]
fn test_install_reports_invalid_runtime_configuration() {
    let (_dir, mut state) = state("../private");
    state.set_token_realm(Signer::new(b"key", "peryx"), 300).unwrap();

    assert_eq!(
        install_auth(&mut state, &publisher()),
        Err("trusted publishing is misconfigured".to_owned())
    );
}

fn install_auth(state: &mut AppState, values: &toml::Table) -> Result<(), String> {
    install(&mut state.auth_install_context()?, values)
}

fn state(route: &str) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        vec![
            Index {
                name: "hosted".to_owned(),
                route: route.to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Hosted { volatile: false },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "root-pypi".to_owned(),
                route: "root/pypi".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![0],
                    write_target: Some(0),
                },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "read-only".to_owned(),
                route: "read-only".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![0],
                    write_target: None,
                },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
        ],
    );
    (dir, state)
}
