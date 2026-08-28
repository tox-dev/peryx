use std::collections::BTreeSet;

use peryx_core::{DefaultIndex, DefaultIndexKind};
use peryx_driver::serving::{
    CompiledEcosystemSettings, DistributedRuntime, EcosystemAuth as _, EcosystemBrowse as _, EcosystemConfig as _,
    EcosystemRegistration as _, EcosystemRuntime, PluginAuthConfig, PluginIndexConfig,
};
use peryx_driver::state::AppState;
use peryx_index::IndexKind;
use rstest::rstest;
use utoipa::openapi::PathsBuilder;

use crate::{ECOSYSTEM, OciPlugin, registration};
use peryx_plugin_registry::PluginAuthRegistration;

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    (
        dir,
        AppState::new(
            meta,
            blobs,
            60,
            vec![super::oci_index("oci", "oci", IndexKind::Hosted { volatile: true })],
        ),
    )
}

#[test]
fn plugin_exposes_its_contract() {
    let plugin = OciPlugin;

    assert_eq!(plugin.ecosystem(), ECOSYSTEM);
    assert_eq!(plugin.absolute_prefixes(), &["/v2/"]);
    let protocol = plugin.driver();
    let driver = protocol.absolute().unwrap();
    assert_eq!(driver.prefixes(), &["/v2/"]);
    assert!(
        plugin
            .compile_index_settings("oci", &toml::Table::new())
            .unwrap()
            .is_some()
    );
    let registration = registration();
    assert_eq!(
        (
            registration.distributed_runtime.is_some(),
            registration.rate_limit_principal.is_some(),
            registration.client_discovery.is_some(),
        ),
        (true, true, true),
    );
    assert_eq!(
        peryx_driver::serving::EcosystemBrowse::paths(registration.browse.unwrap()),
        [
            "/+ui/browse",
            "/+ui/projects",
            "/+ui/project",
            "/+ui/manifest",
            "/+ui/members",
            "/+ui/member",
        ]
    );
    assert_eq!(
        (
            registration.registration.ecosystem(),
            registration.auth.is_some(),
            registration.browse.is_some(),
            registration.snippets.is_none(),
            registration.priority,
        ),
        (ECOSYSTEM, true, true, true, 1)
    );
}

#[test]
fn plugin_auth_uses_only_shared_settings() {
    assert!(matches!(registration().auth, Some(PluginAuthRegistration::Shared(_))));
}

#[test]
fn plugin_auth_installation_needs_no_extension_state() {
    let (_dir, mut state) = state();

    assert_eq!(
        peryx_driver::serving::EcosystemAuth::install(
            &OciPlugin,
            &mut state.auth_install_context().unwrap(),
            &toml::Table::new(),
        ),
        Ok(())
    );
}

#[rstest]
#[case::below_minimum(
    59,
    Some("auth: `token_ttl_secs` must be at least 60 when a signing key and OCI index enable token authentication")
)]
#[case::minimum(60, None)]
fn plugin_validates_oci_token_lifetime(#[case] token_ttl_secs: i64, #[case] expected: Option<&str>) {
    let indexes = [PluginIndexConfig {
        name: "images",
        ecosystem: ECOSYSTEM,
        writable: true,
    }];

    assert_eq!(
        OciPlugin
            .validate(PluginAuthConfig {
                values: &toml::Table::new(),
                signing_key_configured: true,
                token_ttl_secs,
                indexes: &indexes,
            })
            .err()
            .as_deref(),
        expected
    );
}

#[rstest]
#[case::no_signing_key(false, &[PluginIndexConfig {
    name: "images",
    ecosystem: ECOSYSTEM,
    writable: true,
}])]
#[case::no_oci_index(true, &[])]
fn plugin_accepts_short_lifetime_without_oci_token_service(
    #[case] signing_key_configured: bool,
    #[case] indexes: &[PluginIndexConfig<'_>],
) {
    assert_eq!(
        OciPlugin.validate(PluginAuthConfig {
            values: &toml::Table::new(),
            signing_key_configured,
            token_ttl_secs: 1,
            indexes,
        }),
        Ok(())
    );
}

#[test]
fn registration_builds_the_driver_only_after_activation() {
    let registry = peryx_plugin_registry::PluginRegistry::new(vec![registration()]).unwrap();

    assert!(registry.protocol(&ECOSYSTEM).is_none());
    assert!(registry.activate([ECOSYSTEM]).unwrap().protocol(&ECOSYSTEM).is_some());
}

#[test]
fn plugin_exposes_the_default_oci_stack() {
    assert_eq!(
        OciPlugin.default_indexes(),
        [
            DefaultIndex {
                name: "dockerhub",
                route: "dockerhub",
                ecosystem: ECOSYSTEM,
                kind: DefaultIndexKind::Cached {
                    upstream: "https://registry-1.docker.io",
                },
            },
            DefaultIndex {
                name: "images",
                route: "images",
                ecosystem: ECOSYSTEM,
                kind: DefaultIndexKind::Hosted,
            },
            DefaultIndex {
                name: "root-oci",
                route: "root/oci",
                ecosystem: ECOSYSTEM,
                kind: DefaultIndexKind::Virtual {
                    layers: &["images", "dockerhub"],
                    write_target: "images",
                },
            },
        ]
    );
}

#[test]
fn plugin_client_endpoint_builds_the_nested_index_route() {
    assert_eq!(
        peryx_driver::serving::ClientDiscovery::client_endpoint(&OciPlugin, "private/team"),
        "/v2/private/team/"
    );
}

#[test]
fn plugin_exposes_policy_compilation_without_dry_run() {
    let registry = peryx_plugin_registry::PluginRegistry::new(vec![registration()])
        .unwrap()
        .activate([ECOSYSTEM])
        .unwrap();
    let policy = "max_tags_per_repository = 3".parse::<toml::Table>().unwrap();

    assert!(
        !registry
            .drivers()
            .get_policy(&ECOSYSTEM)
            .unwrap()
            .compile_policy(&policy)
            .unwrap()
            .is_empty()
    );
    assert!(registry.drivers().get_policy_dry_run(&ECOSYSTEM).is_none());
}

#[test]
fn plugin_policy_compilation_applies_oci_limits_without_artifact_rules() {
    let registry = peryx_plugin_registry::PluginRegistry::new(vec![registration()])
        .unwrap()
        .activate([ECOSYSTEM])
        .unwrap();
    let capabilities = registry
        .drivers()
        .get_policy(&ECOSYSTEM)
        .unwrap()
        .compile_policy(&"max_tags_per_repository = 3".parse().unwrap())
        .unwrap();
    let policy = peryx_policy::Policy::default().with_capabilities(capabilities);

    assert_eq!(policy.max_groups_per_resource(), Some(3));
    assert!(policy.active());
}

#[test]
fn plugin_policy_rejects_unknown_and_invalid_values() {
    let registry = peryx_plugin_registry::PluginRegistry::new(vec![registration()])
        .unwrap()
        .activate([ECOSYSTEM])
        .unwrap();
    let policy = registry.drivers().get_policy(&ECOSYSTEM).unwrap();

    assert_eq!(
        policy.compile_policy(&"unknown = 1".parse().unwrap()).unwrap_err(),
        "unknown field `unknown` in `[index.policy]`"
    );
    assert!(
        policy
            .compile_policy(&"max_tags_per_repository = \"many\"".parse().unwrap())
            .unwrap_err()
            .contains("invalid type")
    );
}

#[tokio::test]
async fn plugin_browse_requires_an_installed_driver() {
    let (_dir, state) = state();
    let response = OciPlugin
        .dispatch(
            std::sync::Arc::new(state),
            axum::http::Request::builder()
                .uri("/+ui/projects?index=oci")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;

    assert_eq!(response.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn plugin_openapi_contains_every_oci_surface() {
    let paths =
        serde_json::to_value(peryx_driver::serving::EcosystemOpenApi::paths(&OciPlugin, PathsBuilder::new()).build())
            .unwrap();
    let actual: BTreeSet<&str> = paths.as_object().unwrap().keys().map(String::as_str).collect();

    assert_eq!(
        actual,
        BTreeSet::from([
            "/v2/",
            "/v2/{name}/blobs/uploads/",
            "/v2/{name}/blobs/uploads/{session}",
            "/v2/{name}/blobs/{digest}",
            "/v2/{name}/blobs/{digest}/contents",
            "/v2/{name}/manifests/{reference}",
            "/v2/{name}/manifests/{reference}/restore",
            "/v2/{name}/referrers/{digest}",
            "/v2/{name}/tags/list",
        ])
    );
}

#[test]
fn plugin_reports_invalid_index_settings_with_the_index_name() {
    let plugin = OciPlugin;
    let settings = "unknown = true".parse::<toml::Table>().unwrap();

    assert_eq!(
        plugin.compile_index_settings("private", &settings).unwrap_err(),
        "compile settings for private: unknown field `unknown` in `[index.settings]`"
    );
}

#[test]
fn plugin_installs_local_and_distributed_drivers() {
    let plugin = OciPlugin;
    let (_dir, mut state) = state();
    let settings = plugin
        .compile_index_settings("oci", &toml::Table::new())
        .unwrap()
        .unwrap();

    EcosystemRuntime::install(
        &plugin,
        &mut state.runtime_install_context().unwrap(),
        &[("oci", &settings)],
    )
    .unwrap();
    DistributedRuntime::install(
        &plugin,
        &mut state.distributed_install_context().unwrap(),
        &[("oci", &settings)],
    )
    .unwrap();

    assert_eq!(
        state
            .absolute_mounts()
            .map(|(prefix, driver)| (prefix, driver.ecosystem()))
            .collect::<Vec<_>>(),
        vec![("/v2/", ECOSYSTEM)]
    );
}

#[test]
fn plugin_skips_local_install_without_an_oci_index() {
    let plugin = OciPlugin;
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    );

    EcosystemRuntime::install(&plugin, &mut state.runtime_install_context().unwrap(), &[]).unwrap();

    assert_eq!(state.absolute_mounts().count(), 0);
}

#[test]
fn plugin_rejects_settings_compiled_for_another_type() {
    let plugin = OciPlugin;
    let (_dir, mut state) = state();
    let settings = CompiledEcosystemSettings::new(ECOSYSTEM, ());

    assert_eq!(
        EcosystemRuntime::install(
            &plugin,
            &mut state.runtime_install_context().unwrap(),
            &[("oci", &settings)],
        )
        .unwrap_err(),
        "compiled settings for oci have the wrong type"
    );
}
