use super::*;
use crate::app;
use crate::cli::{PolicyCommand, PolicyDryRunArgs};

#[test]
fn test_policy_dry_run_reports_blocked_cached_file() {
    let (_dir, mut config, _digest) = cache_fixture();
    config.indexes[0].policy.block_projects = vec!["flask".to_owned()];
    let mut out = Vec::new();

    app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: Some("pypi".to_owned()),
            project: Some("Flask".to_owned()),
        }),
        &mut out,
    )
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("action\tindex\tproject\tfilename\tversion\trule\tfield\treason\n"));
    assert!(text.contains("serve\tpypi\tflask\t\t\tproject-block-list\tproject\tproject \"flask\" is blocked\n"));
}

#[test]
fn test_policy_dry_run_reports_blocked_upload() {
    let (_dir, mut config, digest) = cache_fixture();
    MetaStore::open(config.data_dir.join("peryx.redb"))
        .unwrap()
        .put_upload("hosted", "pkg", "pkg-1.0.whl", &uploaded_record_json(&digest))
        .unwrap();
    config.indexes[1].policy.max_file_size_bytes = Some(2);
    let mut out = Vec::new();

    app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: Some("hosted".to_owned()),
            project: Some("pkg".to_owned()),
        }),
        &mut out,
    )
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("upload\thosted\tpkg\tpkg-1.0.whl\t\tmax-file-size\tsize\tfile size 3 exceeds limit 2\n"),
        "{text}"
    );
}

#[test]
fn test_policy_dry_run_skips_allowed_upload() {
    let (_dir, config, digest) = cache_fixture();
    MetaStore::open(config.data_dir.join("peryx.redb"))
        .unwrap()
        .put_upload("hosted", "pkg", "pkg-1.0.whl", &uploaded_record_json(&digest))
        .unwrap();
    let mut out = Vec::new();

    app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: Some("hosted".to_owned()),
            project: Some("pkg".to_owned()),
        }),
        &mut out,
    )
    .unwrap();

    assert_eq!(
        String::from_utf8(out).unwrap(),
        "action\tindex\tproject\tfilename\tversion\trule\tfield\treason\n"
    );
}

#[test]
fn test_policy_dry_run_skips_filtered_project() {
    let (_dir, mut config, _digest) = cache_fixture();
    config.indexes[0].policy.block_projects = vec!["flask".to_owned()];
    let mut out = Vec::new();

    app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: Some("pypi".to_owned()),
            project: Some("django".to_owned()),
        }),
        &mut out,
    )
    .unwrap();

    assert_eq!(
        String::from_utf8(out).unwrap(),
        "action\tindex\tproject\tfilename\tversion\trule\tfield\treason\n"
    );
}

#[test]
fn test_policy_dry_run_skips_unmatched_upload_records() {
    let (_dir, mut config, digest) = cache_fixture();
    config.indexes[1].policy.max_file_size_bytes = Some(2);
    let db_path = config.data_dir.join("peryx.redb");
    raw_insert_bytes(&db_path, "uploads", "loose", b"not json");
    raw_insert_bytes(
        &db_path,
        "uploads",
        "foreign/pkg/pkg-1.0.whl",
        &uploaded_record_json(&digest),
    );
    raw_insert_bytes(&db_path, "uploads", "hosted/pkg/pkg-1.0.whl", b"not json");
    let mut out = Vec::new();

    app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: None,
            project: Some("other".to_owned()),
        }),
        &mut out,
    )
    .unwrap();

    assert_eq!(
        String::from_utf8(out).unwrap(),
        "action\tindex\tproject\tfilename\tversion\trule\tfield\treason\n"
    );
}

#[test]
fn test_policy_dry_run_reports_upload_write_errors() {
    let (_dir, mut config, digest) = cache_fixture();
    MetaStore::open(config.data_dir.join("peryx.redb"))
        .unwrap()
        .put_upload("hosted", "pkg", "pkg-1.0.whl", &uploaded_record_json(&digest))
        .unwrap();
    config.indexes[1].policy.max_file_size_bytes = Some(2);
    let mut out = FailOnText {
        needle: "max-file-size",
        seen: String::new(),
    };

    let err = app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: Some("hosted".to_owned()),
            project: Some("pkg".to_owned()),
        }),
        &mut out,
    )
    .unwrap_err();

    assert!(err.to_string().contains("scan upload records"));
}
