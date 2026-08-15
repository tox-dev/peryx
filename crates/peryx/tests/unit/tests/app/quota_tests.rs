use peryx_storage::meta::{AccountingClass, MetaStore, NewQuotaReservation, QuotaLimits};
use rstest::rstest;

use super::*;
use crate::app::tests::{bounded_output, config_at, runtime_args};
use crate::cli::QuotaListArgs;

const LIST_HEADER: &str = "repository\tecosystem\tused_bytes\treserved_bytes\tbyte_limit\tremaining_bytes\tresources\tresource_limit\taudit\n";

#[test]
fn test_quota_public_entrypoint_reports_a_missing_store() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("missing"),
        ..Config::default()
    };

    let error = quota(&config, &list_command(), &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("open metadata store"), "{error:#}");
}

#[test]
fn test_quota_lists_committed_and_reserved_usage() {
    let (_dir, config) = quota_config();
    let mut output = Vec::new();

    quota_with_plugins(&config, &crate::tests::support::plugins(), &list_command(), &mut output).unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.starts_with(LIST_HEADER), "{output}");
    assert!(
        output.contains("main\tcore\t3000\t500\t10000\t6500\t1\t5\tfalse\n"),
        "{output}"
    );
}

#[rstest]
#[case::list_header(list_command(), 0)]
#[case::list_row(list_command(), LIST_HEADER.len())]
#[case::inspect(inspect_command("main"), 0)]
fn test_quota_propagates_output_failures(#[case] command: QuotaCommand, #[case] capacity: usize) {
    let (_dir, config) = quota_config();

    let error = quota_with_plugins(
        &config,
        &crate::tests::support::plugins(),
        &command,
        &mut bounded_output(capacity),
    )
    .unwrap_err();

    assert!(error.to_string().contains("failed to write whole buffer"), "{error:#}");
}

#[test]
fn test_quota_inspects_one_repository() {
    let (_dir, config) = quota_config();
    let mut output = Vec::new();

    quota_with_plugins(
        &config,
        &crate::tests::support::plugins(),
        &inspect_command("main"),
        &mut output,
    )
    .unwrap();

    let status: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        status,
        serde_json::json!({
            "repository": "main",
            "ecosystem": "core",
            "limits": {
                "max_artifact_bytes": null,
                "max_resource_bytes": null,
                "max_accounted_bytes": 10000,
                "max_resources": 5,
                "max_groups_per_resource": null,
                "audit": false
            },
            "artifact_bytes": {"committed": 3000, "reserved": 500, "limit": null, "remaining": null},
            "accounted_bytes": {"committed": 3000, "reserved": 500, "limit": 10000, "remaining": 6500},
            "resources": {"committed": 1, "reserved": 1, "limit": 5, "remaining": 3}
        })
    );
}

#[test]
fn test_quota_inspect_rejects_an_unknown_index() {
    let (_dir, config) = quota_config();

    let error = quota_with_plugins(
        &config,
        &crate::tests::support::plugins(),
        &inspect_command("missing"),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert_eq!(error.to_string(), "unknown index \"missing\"");
}

#[test]
fn test_quota_reports_a_counter_read_failure() {
    let (_dir, config) = quota_config();
    let database = redb::Database::open(config.data_dir.join("peryx.redb")).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .delete_table(redb::TableDefinition::<&str, &[u8]>::new("quota_usage"))
        .unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, u64>::new("quota_usage"))
        .unwrap();
    transaction.commit().unwrap();
    drop(database);

    let error = quota_with_plugins(
        &config,
        &crate::tests::support::plugins(),
        &inspect_command("main"),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("read quota counters for \"main\""),
        "{error:#}"
    );
}

fn inspect_command(index: &str) -> QuotaCommand {
    QuotaCommand::Inspect(QuotaInspectArgs {
        runtime: runtime_args(),
        index: index.to_owned(),
    })
}

const fn list_command() -> QuotaCommand {
    QuotaCommand::List(QuotaListArgs {
        runtime: runtime_args(),
    })
}

fn quota_config() -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config_at(&dir);
    config.indexes[0].policy.max_accounted_bytes = Some(10_000);
    config.indexes[0].policy.max_resources = Some(5);
    let meta = MetaStore::open(config.data_dir.join("peryx.redb")).unwrap();
    let committed = meta
        .reserve_quota(
            NewQuotaReservation {
                repository: "main",
                resource: Some("pkg"),
                group: Some("1.0"),
                digest: "sha256:aaaa",
                bytes: 3000,
                class: AccountingClass::Hosted,
                created_at_unix: 0,
            },
            QuotaLimits::default(),
        )
        .unwrap();
    meta.commit_quota_reservation(committed.id).unwrap();
    meta.reserve_quota(
        NewQuotaReservation {
            repository: "main",
            resource: Some("pkg2"),
            group: Some("1.0"),
            digest: "sha256:bbbb",
            bytes: 500,
            class: AccountingClass::Hosted,
            created_at_unix: 0,
        },
        QuotaLimits::default(),
    )
    .unwrap();
    (dir, config)
}
