use peryx_driver::retention::encode_cursor;
use peryx_policy::{RetentionFrontier, RetentionSummary};
use rstest::rstest;

use super::*;
use crate::app::tests::{bounded_output, config_at, plugins, plugins_without_retention, runtime_args};

const HEADER: &str = "action\tresource\tgroup\tartifact\tdigest\tclass\tvisibility\tbytes\trule\n";

#[test]
fn test_retention_public_entrypoint_reports_a_missing_store() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("missing"),
        ..Config::default()
    };
    let index = config.indexes[0].name.clone();

    let error = retention(
        &config,
        &RetentionCommand::DryRun(dry_run_args(&index)),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("open metadata store"), "{error:#}");
}

#[test]
fn test_retention_dry_run_writes_candidates_summary_and_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);
    let mut args = dry_run_args("main");
    args.limit = Some(1);
    let mut output = Vec::new();

    retention_with_plugins(&config, &plugins(), &RetentionCommand::DryRun(args), &mut output).unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.starts_with(HEADER), "{output}");
    assert!(
        output.contains("retain\titem\t2.0\titem-2.0.bin\tsha-2.0\thosted\tactive\t1024\t\n"),
        "{output}"
    );
    assert!(output.contains("summary\tpolicy_version="), "{output}");
    assert!(output.contains("next-cursor\t"), "{output}");
}

#[test]
fn test_retention_dry_run_resumes_after_a_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);
    let mut args = dry_run_args("main");
    args.limit = Some(1);
    let mut first = Vec::new();
    retention_with_plugins(&config, &plugins(), &RetentionCommand::DryRun(args.clone()), &mut first).unwrap();
    args.cursor = String::from_utf8(first)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("next-cursor\t"))
        .map(str::to_owned);
    let mut output = Vec::new();

    retention_with_plugins(&config, &plugins(), &RetentionCommand::DryRun(args), &mut output).unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("item-1.0.bin"), "{output}");
    assert!(!output.contains("item-2.0.bin"), "{output}");
}

#[test]
fn test_retention_dry_run_applies_a_rules_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);
    let rules = dir.path().join("rules.toml");
    std::fs::write(
        &rules,
        "[[expire]]\nselector = \"resource-prefix\"\nprefix = \"ITEM\"\n",
    )
    .unwrap();
    let mut args = dry_run_args("main");
    args.rules = Some(rules);
    let mut output = Vec::new();

    retention_with_plugins(&config, &plugins(), &RetentionCommand::DryRun(args), &mut output).unwrap();

    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("remove\titem\t1.0\titem-1.0.bin\tsha-1.0\thosted\tactive\t1024\tresource-prefix\n")
    );
}

#[test]
fn test_retention_export_writes_identity_before_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);
    let mut output = Vec::new();

    retention_with_plugins(&config, &plugins(), &export_command(None), &mut output).unwrap();

    let lines = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("\"summary\""), "{}", lines[0]);
    assert!(lines[1].contains("\"artifact\":\"item-2.0.bin\""), "{}", lines[1]);
}

#[test]
fn test_retention_export_rejects_a_stale_cursor_before_output() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);
    let ecosystem = config
        .indexes
        .iter()
        .find(|index| index.name == "main")
        .unwrap()
        .ecosystem
        .as_str();
    let cursor = encode_cursor(
        "main",
        ecosystem,
        Some(42),
        0,
        RetentionSummary {
            policy_version: 999,
            frontier: RetentionFrontier::default(),
        },
    );
    let mut output = Vec::new();

    let error = retention_with_plugins(&config, &plugins(), &export_command(Some(cursor)), &mut output).unwrap_err();

    assert_eq!(
        error.to_string(),
        "the plan cursor is stale: the repository changed since it was issued"
    );
    assert!(output.is_empty());
}

#[test]
fn test_retention_rejects_a_cursor_from_another_repository() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);
    let ecosystem = config
        .indexes
        .iter()
        .find(|index| index.name == "main")
        .unwrap()
        .ecosystem
        .as_str();
    let cursor = encode_cursor(
        "other",
        ecosystem,
        Some(42),
        0,
        RetentionSummary {
            policy_version: 0,
            frontier: RetentionFrontier::default(),
        },
    );
    let mut args = dry_run_args("main");
    args.cursor = Some(cursor);
    let mut output = Vec::new();

    let error = retention_with_plugins(&config, &plugins(), &RetentionCommand::DryRun(args), &mut output).unwrap_err();

    assert_eq!(
        error.to_string(),
        "the plan cursor is stale: the repository changed since it was issued"
    );
    assert!(output.is_empty());
}

#[test]
fn test_retention_reports_missing_capability() {
    let dir = tempfile::tempdir().unwrap();
    let plain_plugins = plugins_without_retention();
    let plain_config = Config {
        data_dir: dir.path().join("plain"),
        ..Config::with_plugins(&plain_plugins)
    };
    std::fs::create_dir(&plain_config.data_dir).unwrap();
    drop(peryx_storage::meta::MetaStore::open(plain_config.data_dir.join("peryx.redb")).unwrap());
    let unsupported = retention_with_plugins(
        &plain_config,
        &plain_plugins,
        &RetentionCommand::DryRun(dry_run_args("plain")),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert_eq!(
        unsupported.to_string(),
        "the ecosystem does not support retention planning"
    );
}

#[test]
fn test_retention_rejects_invalid_rules_and_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);
    let rules = dir.path().join("rules.toml");
    std::fs::write(&rules, "[[keep]]\nselector = \"missing\"\n").unwrap();
    let mut invalid_rules = dry_run_args("main");
    invalid_rules.rules = Some(rules);
    let mut invalid_cursor = dry_run_args("main");
    invalid_cursor.cursor = Some("not-a-cursor".to_owned());

    let rules_error = retention_with_plugins(
        &config,
        &plugins(),
        &RetentionCommand::DryRun(invalid_rules),
        &mut Vec::new(),
    )
    .unwrap_err();
    let cursor_error = retention_with_plugins(
        &config,
        &plugins(),
        &RetentionCommand::DryRun(invalid_cursor),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert!(rules_error.to_string().contains("parse rules file"), "{rules_error:#}");
    assert!(
        cursor_error.to_string().contains("invalid retention plan cursor"),
        "{cursor_error:#}"
    );
}

#[test]
fn test_retention_reports_an_unreadable_rules_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);
    let mut args = dry_run_args("main");
    args.rules = Some(config.data_dir.join("missing.toml"));

    let error =
        retention_with_plugins(&config, &plugins(), &RetentionCommand::DryRun(args), &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("read rules file"), "{error:#}");
}

#[test]
fn test_retention_propagates_decision_output_failures() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);

    let dry_run_error = retention_with_plugins(
        &config,
        &plugins(),
        &RetentionCommand::DryRun(dry_run_args("main")),
        &mut bounded_output(HEADER.len()),
    )
    .unwrap_err();
    let mut complete_export = Vec::new();
    retention_with_plugins(&config, &plugins(), &export_command(None), &mut complete_export).unwrap();
    let header_bytes = complete_export.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    let export_error = retention_with_plugins(
        &config,
        &plugins(),
        &export_command(None),
        &mut bounded_output(header_bytes),
    )
    .unwrap_err();

    assert!(dry_run_error.to_string().contains("failed to write whole buffer"));
    assert!(export_error.to_string().contains("failed to write whole buffer"));
}

#[rstest]
#[case::dry_run(RetentionCommand::DryRun(dry_run_args("main")))]
#[case::export(export_command(None))]
fn test_retention_propagates_header_output_failures(#[case] command: RetentionCommand) {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);

    let error = retention_with_plugins(&config, &plugins(), &command, &mut bounded_output(0)).unwrap_err();

    assert!(error.to_string().contains("failed to write whole buffer"), "{error:#}");
}

fn dry_run_args(index: &str) -> RetentionDryRunArgs {
    RetentionDryRunArgs {
        runtime: runtime_args(),
        index: index.to_owned(),
        rules: None,
        limit: None,
        cursor: None,
    }
}

fn export_command(cursor: Option<String>) -> RetentionCommand {
    RetentionCommand::Export(RetentionExportArgs {
        runtime: runtime_args(),
        index: "main".to_owned(),
        rules: None,
        cursor,
    })
}
