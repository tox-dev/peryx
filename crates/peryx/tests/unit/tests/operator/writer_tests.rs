use std::path::Path;

use peryx_storage::meta::MetaStore;

use crate::config::Config;
use crate::operator;

#[test]
fn test_claim_writer_creates_missing_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("missing");
    let config = writer_config(&data_dir);
    let mut out = Vec::new();

    operator::claim_writer(&config, &mut out).unwrap();

    let meta = MetaStore::open_existing(data_dir.join("peryx.redb")).unwrap();
    assert_eq!(
        (meta.writer_identity().unwrap(), out),
        (Some("writer-a".to_owned()), b"writer\twriter-a\n".to_vec())
    );
}

#[test]
fn test_promote_writer_changes_the_stored_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = writer_config(dir.path());
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.claim_writer_identity("writer-a").unwrap();
    drop(meta);
    let mut out = Vec::new();

    operator::promote_writer(&config, "writer-b", &mut out).unwrap();

    let meta = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    assert_eq!(
        (meta.writer_identity().unwrap(), out),
        (Some("writer-b".to_owned()), b"writer\twriter-a\twriter-b\n".to_vec())
    );
}

#[test]
fn test_claim_writer_requires_a_configured_identity() {
    let error = operator::claim_writer(&Config::default(), &mut Vec::new()).unwrap_err();

    assert_eq!(
        error.to_string(),
        "writer identity is not configured; set `writer_identity` to the writer this replica follows"
    );
}

#[test]
fn test_promote_writer_requires_a_configured_identity() {
    let error = operator::promote_writer(&Config::default(), "writer-b", &mut Vec::new()).unwrap_err();

    assert_eq!(
        error.to_string(),
        "writer identity is not configured; set `writer_identity` to the active writer"
    );
}

#[test]
fn test_claim_writer_reports_data_directory_errors() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::write(&data_dir, []).unwrap();
    let config = writer_config(&data_dir);

    let error = operator::claim_writer(&config, &mut Vec::new()).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!("create data directory {}", data_dir.display())
    );
}

#[test]
fn test_promote_writer_reports_store_open_errors() {
    let dir = tempfile::tempdir().unwrap();
    let config = writer_config(&dir.path().join("missing"));

    let error = operator::promote_writer(&config, "writer-b", &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("open metadata store"), "{error:#}");
}

#[test]
fn test_promote_writer_rejects_a_stale_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = writer_config(dir.path());
    operator::claim_writer(&config, &mut Vec::new()).unwrap();
    let mut stale = config;
    stale.writer_identity = Some("stale".to_owned());

    let error = operator::promote_writer(&stale, "writer-b", &mut Vec::new()).unwrap_err();

    assert!(
        format!("{error:#}").contains("metadata store writer is Some(\"writer-a\")"),
        "{error:#}"
    );
}

#[test]
fn test_claim_writer_propagates_output_errors() {
    let dir = tempfile::tempdir().unwrap();
    let config = writer_config(dir.path());
    let mut out: &mut [u8] = &mut [];

    let error = operator::claim_writer(&config, &mut out).unwrap_err();

    assert_eq!(
        error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
        Some(std::io::ErrorKind::WriteZero)
    );
}

#[test]
fn test_promote_writer_propagates_output_errors() {
    let dir = tempfile::tempdir().unwrap();
    let config = writer_config(dir.path());
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.claim_writer_identity("writer-a").unwrap();
    drop(meta);
    let mut out: &mut [u8] = &mut [];

    let error = operator::promote_writer(&config, "writer-b", &mut out).unwrap_err();

    assert_eq!(
        error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
        Some(std::io::ErrorKind::WriteZero)
    );
}

#[test]
fn test_claim_writer_rejects_an_invalid_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        writer_identity: Some(String::new()),
        ..writer_config(dir.path())
    };

    let error = operator::claim_writer(&config, &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), "claim writer identity \"\"");
}

fn writer_config(data_dir: &Path) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        ..Config::default()
    }
}
