use peryx_driver::AppState;
use peryx_driver::serving::PluginIndexConfig;
use peryx_identity::Signer;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

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
    let (_dir, mut state) = state();

    install_auth(&mut state, &auth_defaults()).unwrap();

    assert!(!crate::trusted_publishing_enabled(&state));
    assert_eq!(state.http_routes().count(), 0);
}

#[test]
fn test_install_requires_the_validated_signing_key() {
    let (_dir, mut state) = state();

    assert_eq!(
        install_auth(&mut state, &publisher()),
        Err("auth: `signing_key` is required when trusted publishers are configured".to_owned())
    );
}

#[test]
fn test_install_registers_runtime_and_routes() {
    let (_dir, mut state) = state();
    state.set_token_realm(Signer::new(b"key", "peryx"), 300).unwrap();

    install_auth(&mut state, &publisher()).unwrap();

    assert!(crate::trusted_publishing_enabled(&state));
    assert_eq!(state.http_routes().count(), 1);
}

#[test]
fn test_install_reports_invalid_runtime_configuration() {
    let (_dir, mut state) = state();
    state.set_token_realm(Signer::new(b"key", "peryx"), 300).unwrap();
    let mut values = publisher();
    values["trusted_publisher"].as_array_mut().unwrap()[0]["repository"] = toml::Value::String("../private".to_owned());

    assert_eq!(
        install_auth(&mut state, &values),
        Err("trusted publishing is misconfigured".to_owned())
    );
}

fn install_auth(state: &mut AppState, values: &toml::Table) -> Result<(), String> {
    install(&mut state.auth_install_context()?, values)
}

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        Vec::new(),
    );
    (dir, state)
}
