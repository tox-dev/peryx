use peryx_storage::meta::MetaStore;

use crate::config::Config;
use crate::operator;

fn claimed(identity: &str) -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.claim_writer_identity(identity).unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some(identity.to_owned()),
        ..Config::default()
    };
    (dir, config)
}

#[test]
fn test_promote_writer_replaces_the_configured_identity() {
    let (dir, config) = claimed("writer-a");
    let mut out = Vec::new();

    operator::promote_writer(&config, "writer-b", &mut out).unwrap();

    let meta = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    assert_eq!(meta.writer_identity().unwrap().as_deref(), Some("writer-b"));
    assert_eq!(String::from_utf8(out).unwrap(), "writer\twriter-a\twriter-b\n");
}

#[test]
fn test_promote_writer_requires_a_configured_identity() {
    let error = operator::promote_writer(&Config::default(), "writer-b", &mut Vec::new()).unwrap_err();
    assert!(
        error.to_string().contains("writer identity is not configured"),
        "{error}"
    );
}

#[test]
fn test_promote_writer_rejects_a_stale_configured_identity() {
    let (_dir, mut config) = claimed("writer-a");
    config.writer_identity = Some("stale".to_owned());

    let error = operator::promote_writer(&config, "writer-b", &mut Vec::new()).unwrap_err();

    let message = format!("{error:#}");
    assert!(
        message.contains("promote writer from \"stale\" to \"writer-b\""),
        "{message}"
    );
    assert!(
        message.contains("metadata store writer is Some(\"writer-a\")"),
        "{message}"
    );
}

#[test]
fn test_promote_writer_reports_a_missing_metadata_store() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        ..Config::default()
    };

    let error = operator::promote_writer(&config, "writer-b", &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("open metadata store"), "{error}");
}

#[test]
fn test_promote_writer_rejects_an_empty_replacement() {
    let (_dir, config) = claimed("writer-a");

    let error = operator::promote_writer(&config, "", &mut Vec::new()).unwrap_err();

    assert!(format!("{error:#}").contains("writer identity cannot be empty"));
}
