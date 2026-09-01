use std::collections::BTreeMap;

use peryx_core::Ecosystem;
use peryx_driver::serving::MirrorAction;
use peryx_plugin_registry::PluginRegistry;
use peryx_policy::PolicyConfig;
use rstest::rstest;

use super::{run, run_with_plugins};
use crate::cli::{
    PrefetchCommand, PrefetchOptions, PrefetchPlanArgs, PrefetchSyncArgs, PrefetchVerifyArgs, RuntimeArgs,
};
use crate::config::{
    Config, IndexConfig, IndexKind, PrefetchConfig, UpstreamConfig, UpstreamRoutingConfig, UpstreamTlsConfig,
};
use crate::tests::support::{plugins, plugins_without_retention};

#[rstest]
#[case::plan(MirrorAction::Plan, "cache-route", "Plan\tcache-route\t1\t1\n")]
#[case::sync(MirrorAction::Sync, "virtual", "Sync\tvirtual\t1\t1\n")]
#[case::verify(MirrorAction::Verify, "cached", "Verify\tcached\t1\t1\n")]
#[tokio::test]
async fn test_run_with_plugins_forwards_action_index_and_options(
    #[case] action: MirrorAction,
    #[case] selector: &str,
    #[case] expected: &str,
) {
    let plugins = plugins();
    let directory = tempfile::tempdir().unwrap();
    let mut output = Vec::new();

    run_with_plugins(
        &config(&directory, &plugins),
        &plugins,
        &command(action, selector, &["limit=3"]),
        &mut output,
    )
    .await
    .unwrap();

    assert_eq!(String::from_utf8(output).unwrap(), expected);
}

#[rstest]
#[case::hosted("hosted", "index \"hosted\" is hosted and has no upstream")]
#[case::no_cached_member("hosted-only", "index \"hosted-only\" has no cached member")]
#[case::multiple_cached_members("two-caches", "index \"two-caches\" has more than one cached member")]
#[tokio::test]
async fn test_run_with_plugins_rejects_indexes_without_one_cached_source(
    #[case] selector: &str,
    #[case] expected: &str,
) {
    let plugins = plugins();
    let directory = tempfile::tempdir().unwrap();

    let error = run_with_plugins(
        &config(&directory, &plugins),
        &plugins,
        &command(MirrorAction::Plan, selector, &[]),
        &mut Vec::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.to_string(), expected);
}

#[rstest]
#[case::missing_separator("mode", "mirror option \"mode\" must be KEY=VALUE")]
#[case::empty_key("=true", "mirror option \"=true\" must be KEY=VALUE")]
#[case::empty_value("mode=", "mirror option \"mode=\" must be KEY=VALUE")]
#[case::invalid_value("mode=all", "invalid value for mirror option \"mode\"")]
#[tokio::test]
async fn test_run_with_plugins_rejects_invalid_overrides(#[case] option: &str, #[case] expected: &str) {
    let plugins = plugins();
    let directory = tempfile::tempdir().unwrap();

    let error = run_with_plugins(
        &config(&directory, &plugins),
        &plugins,
        &command(MirrorAction::Plan, "cached", &[option]),
        &mut Vec::new(),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains(expected), "{error:#}");
}

#[tokio::test]
async fn test_run_with_plugins_rejects_an_unknown_index() {
    let plugins = plugins();
    let directory = tempfile::tempdir().unwrap();

    let error = run_with_plugins(
        &config(&directory, &plugins),
        &plugins,
        &command(MirrorAction::Plan, "missing", &[]),
        &mut Vec::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.to_string(), "unknown cached index \"missing\"");
}

#[tokio::test]
async fn test_run_rejects_an_unknown_index() {
    let directory = tempfile::tempdir().unwrap();

    let error = run(
        &Config {
            data_dir: directory.path().to_path_buf(),
            ..Config::default()
        },
        &command(MirrorAction::Plan, "missing", &[]),
        &mut Vec::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.to_string(), "unknown cached index \"missing\"");
}

#[tokio::test]
async fn test_run_with_plugins_rejects_an_ecosystem_without_mirroring() {
    let plugins = plugins_without_retention();
    let directory = tempfile::tempdir().unwrap();

    let error = run_with_plugins(
        &config(&directory, &plugins),
        &plugins,
        &command(MirrorAction::Plan, "cached", &[]),
        &mut Vec::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.to_string(), "configured ecosystem does not support mirroring");
}

#[tokio::test]
async fn test_run_with_plugins_surfaces_the_driver_error() {
    let plugins = plugins();
    let directory = tempfile::tempdir().unwrap();

    let error = run_with_plugins(
        &config(&directory, &plugins),
        &plugins,
        &command(MirrorAction::Plan, "cached", &["fail=true"]),
        &mut Vec::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.to_string(), "mirror failed");
}

fn command(action: MirrorAction, index: &str, overrides: &[&str]) -> PrefetchCommand {
    let options = PrefetchOptions {
        runtime: RuntimeArgs::default(),
        index: index.to_owned(),
        overrides: overrides.iter().map(|option| (*option).to_owned()).collect(),
    };
    match action {
        MirrorAction::Plan => PrefetchCommand::Plan(PrefetchPlanArgs { options }),
        MirrorAction::Sync => PrefetchCommand::Sync(PrefetchSyncArgs { options }),
        MirrorAction::Verify => PrefetchCommand::Verify(PrefetchVerifyArgs { options }),
    }
}

fn config(directory: &tempfile::TempDir, plugins: &PluginRegistry) -> Config {
    let ecosystem = plugins.default_ecosystem();
    Config {
        data_dir: directory.path().to_path_buf(),
        indexes: vec![
            index(
                ecosystem.clone(),
                "hosted",
                "hosted",
                IndexKind::Hosted { volatile: true },
            ),
            cached_index(ecosystem.clone(), "cached", "cache-route"),
            cached_index(ecosystem.clone(), "second-cache", "second-cache"),
            virtual_index(ecosystem.clone(), "virtual", &["hosted", "cached"]),
            virtual_index(ecosystem.clone(), "hosted-only", &["hosted"]),
            virtual_index(ecosystem, "two-caches", &["cached", "second-cache"]),
        ],
        ..Config::with_plugins(plugins)
    }
}

fn cached_index(ecosystem: Ecosystem, name: &str, route: &str) -> IndexConfig {
    index(
        ecosystem,
        name,
        route,
        IndexKind::Cached {
            routing: UpstreamRoutingConfig {
                upstreams: vec![UpstreamConfig {
                    name: "primary".to_owned(),
                    url: "http://127.0.0.1:1".to_owned(),
                    artifact_url: None,
                    trusted_hosts: Vec::new(),
                    username: None,
                    password: None,
                    token: None,
                    credential_exec: None,
                    credential_refresh: None,
                    tls: UpstreamTlsConfig::default(),
                }],
                fallback: true,
                protected: Vec::new(),
                pins: BTreeMap::default(),
            },
            upstream_concurrency: 0,
            offline: true,
            prefetch: Box::new(PrefetchConfig {
                options: toml::Table::from_iter([("configured".to_owned(), toml::Value::Boolean(true))]),
            }),
        },
    )
}

fn virtual_index(ecosystem: Ecosystem, name: &str, layers: &[&str]) -> IndexConfig {
    index(
        ecosystem,
        name,
        name,
        IndexKind::Virtual {
            layers: layers.iter().map(|layer| (*layer).to_owned()).collect(),
            write_target: None,
        },
    )
}

fn index(ecosystem: Ecosystem, name: &str, route: &str, kind: IndexKind) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: route.to_owned(),
        ecosystem,
        kind,
        anonymous_read: None,
        tokens: Vec::new(),
        policy: PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
    }
}
