use peryx_storage::meta::{AccountingClass, MetaStore, NewQuotaReservation, QuotaLimits};

use super::*;
use crate::app;
use crate::cli::{QuotaCommand, QuotaListArgs};

#[test]
fn test_quota_list_tabulates_every_repository() {
    let (_dir, config) = quota_config();
    let mut output = Vec::new();

    app::quota(
        &config,
        &QuotaCommand::List(QuotaListArgs {
            runtime: runtime_args(),
        }),
        &mut output,
    )
    .unwrap();

    let text = String::from_utf8(output).unwrap();
    assert!(
        text.starts_with(
            "repository\tecosystem\tused_bytes\treserved_bytes\tbyte_limit\tremaining_bytes\tresources\tresource_limit\taudit\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("hosted\tpypi\t3000\t500\t10000\t6500\t1\t5\tfalse\n"),
        "{text}"
    );
    assert!(text.contains("pypi\tpypi\t0\t0\t-\t-\t0\t-\tfalse\n"), "{text}");
}

fn quota_config() -> (tempfile::TempDir, Config) {
    let (dir, mut config, _digest) = cache_fixture();
    config.indexes[1].policy.max_accounted_bytes = Some(10_000);
    config.indexes[1].policy.max_resources = Some(5);
    let meta = MetaStore::open(config.data_dir.join("peryx.redb")).unwrap();
    let committed = meta
        .reserve_quota(
            NewQuotaReservation {
                repository: "hosted",
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
            repository: "hosted",
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
