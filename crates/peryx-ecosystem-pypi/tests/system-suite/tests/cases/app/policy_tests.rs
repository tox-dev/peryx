use super::*;
use crate::app;
use crate::cli::{PolicyCommand, PolicyDryRunArgs};

const POLICY_HEADER: &str = "action\tindex\tresource\tartifact\tgroup\trule\tfield\treason\n";

#[test]
fn test_policy_dry_run_reports_blocked_cached_file() {
    let (_dir, mut config, _digest) = cache_fixture();
    config.indexes[0].policy.block_resources = vec!["flask".to_owned()];
    let mut out = Vec::new();

    app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: Some("pypi".to_owned()),
            resource: Some("Flask".to_owned()),
        }),
        &mut out,
    )
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(POLICY_HEADER));
    assert!(text.contains("serve\tpypi\tflask\t\t\tresource-block-list\tresource\tresource \"flask\" is blocked\n"));
}

#[test]
fn test_policy_dry_run_reports_blocked_upload() {
    let (_dir, mut config, digest) = cache_fixture();
    MetaStore::open(config.data_dir.join("peryx.redb"))
        .unwrap()
        .put_upload("hosted", "pkg", "pkg-1.0.whl", &uploaded_record_json(&digest))
        .unwrap();
    config.indexes[1].policy.max_artifact_size_bytes = Some(2);
    let mut out = Vec::new();

    app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: Some("hosted".to_owned()),
            resource: Some("pkg".to_owned()),
        }),
        &mut out,
    )
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("upload\thosted\tpkg\tpkg-1.0.whl\t\tmax-artifact-size\tsize\tartifact size 3 exceeds limit 2\n"),
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
            resource: Some("pkg".to_owned()),
        }),
        &mut out,
    )
    .unwrap();

    assert_eq!(String::from_utf8(out).unwrap(), POLICY_HEADER);
}

#[test]
fn test_policy_dry_run_skips_filtered_resource() {
    let (_dir, mut config, _digest) = cache_fixture();
    config.indexes[0].policy.block_resources = vec!["flask".to_owned()];
    let mut out = Vec::new();

    app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: Some("pypi".to_owned()),
            resource: Some("django".to_owned()),
        }),
        &mut out,
    )
    .unwrap();

    assert_eq!(String::from_utf8(out).unwrap(), POLICY_HEADER);
}

#[test]
fn test_policy_dry_run_skips_unmatched_upload_records() {
    let (_dir, mut config, digest) = cache_fixture();
    config.indexes[1].policy.max_artifact_size_bytes = Some(2);
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
            resource: Some("other".to_owned()),
        }),
        &mut out,
    )
    .unwrap();

    assert_eq!(String::from_utf8(out).unwrap(), POLICY_HEADER);
}

#[test]
fn test_policy_dry_run_reports_upload_write_errors() {
    let (_dir, mut config, digest) = cache_fixture();
    MetaStore::open(config.data_dir.join("peryx.redb"))
        .unwrap()
        .put_upload("hosted", "pkg", "pkg-1.0.whl", &uploaded_record_json(&digest))
        .unwrap();
    config.indexes[1].policy.max_artifact_size_bytes = Some(2);
    let mut out = bounded_output(POLICY_HEADER.len());

    let err = app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: Some("hosted".to_owned()),
            resource: Some("pkg".to_owned()),
        }),
        &mut out,
    )
    .unwrap_err();

    assert!(err.to_string().contains("preview pypi policy"), "{err}");
}

#[test]
fn test_policy_dry_run_skips_a_record_for_an_unconfigured_index() {
    use peryx_ecosystem_pypi::store::CachedIndex;

    let (_dir, config, _digest) = cache_fixture();
    let store = MetaStore::open(config.data_dir.join("peryx.redb")).unwrap();
    let ghost = CachedIndex {
        source: None,
        last_modified: None,
        etag: None,
        last_serial: None,
        fetched_at_unix: 0,
        content_type: None,
        fresh_secs: None,
        body: Vec::new(),
    };
    store.put_index("ghost/flask", &ghost).unwrap();
    store.put_index("ghostnoslash", &ghost).unwrap();
    drop(store);
    let mut out = Vec::new();
    app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: None,
            resource: None,
        }),
        &mut out,
    )
    .unwrap();
    assert!(
        !String::from_utf8(out).unwrap().contains("ghost"),
        "the ghost record must not appear"
    );
}
