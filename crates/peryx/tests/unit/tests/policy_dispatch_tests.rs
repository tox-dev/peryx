use std::collections::BTreeMap;

use peryx_ecosystem_pypi::store::PypiStore as _;
use peryx_ecosystem_pypi::upload::Uploaded;
use peryx_ecosystem_pypi::{CoreMetadata, File, Provenance, Yanked};
use peryx_storage::meta::MetaStore;

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

/// Each ecosystem's dry run scans only the indexes configured for that ecosystem: an upload
/// violating the pypi `hosted` index's size limit must not go unreported because a different
/// ecosystem's indexes were scanned in its place. `oci` names none of its own indexes `hosted`, so
/// scanning the wrong ecosystem's indexes here can only ever miss the upload, never coincidentally
/// find it under another name.
#[test]
fn test_policy_dry_run_scopes_each_ecosystem_to_its_own_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = crate::compiled_plugins();
    let mut config = config(&dir, Config::default().indexes);
    config
        .indexes
        .iter_mut()
        .find(|index| index.name == "hosted")
        .unwrap()
        .policy
        .max_artifact_size_bytes = Some(2);
    initialize(&config, &plugins);
    let mut hashes = BTreeMap::new();
    hashes.insert("sha256".to_owned(), "0".repeat(64));
    let record = serde_json::to_vec(&Uploaded {
        version: "1.0".to_owned(),
        file: File {
            filename: "pkg-1.0.whl".to_owned(),
            url: "http://localhost/files/pkg-1.0.whl".to_owned(),
            hashes,
            requires_python: None,
            size: Some(3),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
        trashed: None,
    })
    .unwrap();
    MetaStore::open(config.data_dir.join("peryx.redb"))
        .unwrap()
        .put_upload("hosted", "pkg", "pkg-1.0.whl", &record)
        .unwrap();
    let mut output = Vec::new();

    policy_with_plugins(
        &config,
        &plugins,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: RuntimeArgs::default(),
            index: None,
            resource: None,
        }),
        &mut output,
    )
    .unwrap();

    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("upload\thosted\tpkg\tpkg-1.0.whl\t\tmax-artifact-size\tsize\tartifact size 3 exceeds limit 2\n"),
        "expected a size-limit denial scanning the hosted index"
    );
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
