use peryx_storage::meta::MetaStore;

use super::*;
use crate::app::tests::runtime_args;

#[test]
fn test_policy_dispatches_the_dry_run_command() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let command = PolicyCommand::DryRun(PolicyDryRunArgs {
        runtime: runtime_args(),
        index: None,
        resource: None,
    });
    let mut output = Vec::new();

    policy(&config, &command, &mut output).unwrap();

    assert_eq!(
        output,
        b"action\tindex\tresource\tartifact\tgroup\trule\tfield\treason\n"
    );
}

#[test]
fn test_policy_rejects_an_unknown_selected_index() {
    let plugins = crate::tests::support::plugins();
    let command = PolicyCommand::DryRun(PolicyDryRunArgs {
        runtime: runtime_args(),
        index: Some("missing".to_owned()),
        resource: None,
    });

    let error = policy_with_plugins(&Config::with_plugins(&plugins), &plugins, &command, &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), "unknown index \"missing\"");
}
