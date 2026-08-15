use crate::app::policy_with_plugins;
use crate::cli::{PolicyCommand, PolicyDryRunArgs, RuntimeArgs};
use crate::config::{Config, IndexConfig};

fn config(dir: &tempfile::TempDir, indexes: Vec<IndexConfig>) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes,
        ..Config::default()
    }
}

fn command(index: Option<String>) -> PolicyCommand {
    PolicyCommand::DryRun(PolicyDryRunArgs {
        runtime: RuntimeArgs::default(),
        index,
        resource: None,
    })
}

fn initialize(config: &Config, plugins: &peryx_plugin_registry::PluginRegistry) {
    drop(crate::server::build_state_with_plugins(config, plugins).unwrap());
}

fn indexes_with_dry_run(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    enabled: bool,
) -> Vec<IndexConfig> {
    let plugins = plugins
        .activate(config.indexes.iter().map(|index| index.ecosystem.clone()))
        .unwrap();
    config
        .indexes
        .iter()
        .filter(|index| plugins.drivers().get_policy_dry_run(&index.ecosystem).is_some() == enabled)
        .cloned()
        .collect()
}

#[test]
fn test_policy_dry_run_uses_configured_supported_ecosystems() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = crate::compiled_plugins();
    let config = config(&dir, Config::default().indexes);
    initialize(&config, &plugins);
    let mut output = Vec::new();

    policy_with_plugins(&config, &plugins, &command(None), &mut output).unwrap();

    assert_eq!(
        String::from_utf8(output).unwrap(),
        "action\tindex\tresource\tartifact\tgroup\trule\tfield\treason\n"
    );
}

#[test]
fn test_policy_dry_run_rejects_an_explicit_unsupported_ecosystem() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = crate::compiled_plugins();
    let defaults = Config::default();
    let unsupported = indexes_with_dry_run(&defaults, &plugins, false);
    let selected = unsupported.first().unwrap().name.clone();
    let config = config(&dir, unsupported);

    let error = policy_with_plugins(&config, &plugins, &command(Some(selected)), &mut Vec::new()).unwrap_err();

    assert!(
        error.to_string().contains("does not support policy dry-run"),
        "{error:#}"
    );
}

#[test]
fn test_policy_dry_run_rejects_configuration_without_support() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = crate::compiled_plugins();
    let config = config(&dir, indexes_with_dry_run(&Config::default(), &plugins, false));

    let error = policy_with_plugins(&config, &plugins, &command(None), &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), "no configured ecosystem supports policy dry-run");
}

#[test]
fn test_policy_dry_run_accepts_a_supported_index_route() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = crate::compiled_plugins();
    let defaults = Config::default();
    let supported = indexes_with_dry_run(&defaults, &plugins, true);
    let selected = supported.first().unwrap().route.clone();
    let config = config(&dir, supported);
    initialize(&config, &plugins);

    policy_with_plugins(&config, &plugins, &command(Some(selected)), &mut Vec::new()).unwrap();
}
