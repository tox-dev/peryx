use peryx_storage::meta::{AccountingClass, MetaStore, NewQuotaReservation, QuotaLimits};

use super::*;
use crate::app;
use crate::cli::{QuotaCommand, QuotaInspectArgs, QuotaListArgs};

fn quota_config() -> (tempfile::TempDir, Config) {
    let (dir, mut config, _digest) = cache_fixture();
    config.indexes[1].policy.max_accounted_bytes = Some(10_000);
    config.indexes[1].policy.max_projects = Some(5);
    seed(&MetaStore::open(config.data_dir.join("peryx.redb")).unwrap());
    (dir, config)
}

/// Settle 3000 committed and 500 reserved accounted bytes across two projects of the `hosted` store.
fn seed(meta: &MetaStore) {
    let committed = meta
        .reserve_quota(
            NewQuotaReservation {
                repository: "hosted",
                project: Some("pkg"),
                version: Some("1.0"),
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
            repository: "hosted",
            project: Some("pkg2"),
            version: Some("1.0"),
            digest: "sha256:bbbb",
            bytes: 500,
            class: AccountingClass::Hosted,
            created_at_unix: 0,
        },
        QuotaLimits::default(),
    )
    .unwrap();
}

fn list_command() -> QuotaCommand {
    QuotaCommand::List(QuotaListArgs {
        runtime: runtime_args(),
    })
}

fn inspect_command(index: &str) -> QuotaCommand {
    QuotaCommand::Inspect(QuotaInspectArgs {
        runtime: runtime_args(),
        index: index.to_owned(),
    })
}

#[test]
fn test_quota_list_tabulates_every_repository() {
    let (_dir, config) = quota_config();
    let mut out = Vec::new();

    app::quota(&config, &list_command(), &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(
        text.starts_with(
            "repository\tecosystem\tused_bytes\treserved_bytes\tbyte_limit\tremaining_bytes\tprojects\tproject_limit\taudit\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("hosted\tpypi\t3000\t500\t10000\t6500\t1\t5\tfalse\n"),
        "{text}"
    );
    // A cached index configures no repository limits, so its byte and project limits read as absent.
    assert!(text.contains("pypi\tpypi\t0\t0\t-\t-\t0\t-\tfalse\n"), "{text}");
}

#[test]
fn test_quota_inspect_reports_one_repository_as_json() {
    let (_dir, config) = quota_config();
    let mut out = Vec::new();

    app::quota(&config, &inspect_command("hosted"), &mut out).unwrap();

    let status: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(status["repository"], "hosted");
    assert_eq!(status["accounted_bytes"]["committed"], 3000);
    assert_eq!(status["accounted_bytes"]["remaining"], 6500);
    assert_eq!(status["projects"]["limit"], 5);
    assert_eq!(status["file_bytes"]["limit"], serde_json::Value::Null);
}

#[test]
fn test_quota_inspect_rejects_an_unknown_index() {
    let (_dir, config) = quota_config();

    let error = app::quota(&config, &inspect_command("missing"), &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("unknown index \"missing\""), "{error}");
}

#[test]
fn test_quota_read_failure_surfaces_the_repository() {
    let (_dir, config) = quota_config();
    corrupt_quota_table(&config);

    let error = app::quota(&config, &inspect_command("hosted"), &mut Vec::new()).unwrap_err();

    assert!(
        error.to_string().contains("read quota counters for \"hosted\""),
        "{error}"
    );
}

#[test]
fn test_quota_list_propagates_a_header_write_failure() {
    let (_dir, config) = quota_config();

    let error = app::quota(&config, &list_command(), &mut FailImmediately).unwrap_err();

    assert!(error.to_string().contains("write failed"), "{error}");
}

#[test]
fn test_quota_list_propagates_a_row_write_failure() {
    let (_dir, config) = quota_config();
    let mut out = FailOnText {
        needle: "hosted",
        ..Default::default()
    };

    let error = app::quota(&config, &list_command(), &mut out).unwrap_err();

    assert!(error.to_string().contains("write failed"), "{error}");
}

#[test]
fn test_quota_inspect_propagates_a_write_failure() {
    let (_dir, config) = quota_config();

    let error = app::quota(&config, &inspect_command("hosted"), &mut FailImmediately).unwrap_err();

    assert!(error.to_string().contains("write failed"), "{error}");
}

fn corrupt_quota_table(config: &Config) {
    let database = redb::Database::open(config.data_dir.join("peryx.redb")).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .delete_table(redb::TableDefinition::<&str, &[u8]>::new("quota_usage"))
        .unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, u64>::new("quota_usage"))
        .unwrap();
    transaction.commit().unwrap();
}
