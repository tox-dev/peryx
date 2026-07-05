use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest as _, Sha256};
use velodex_ecosystem_pypi::{CoreMetadata, File, Provenance, Yanked, to_json};
use velodex_ecosystem_pypi::upload::Uploaded;
use velodex_storage::blob::{BlobStore, Digest};
use velodex_storage::meta::MetaStore;

use crate::config::{Config, IndexConfig, IndexKind, LogConfig, LogFormat, LogSink};
use crate::operator;

#[test]
fn test_backup_restore_roundtrip_restores_metadata_and_blobs() {
    let (_source, config, content_digest, metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    let restored = root.path().join("restored");
    let mut out = Vec::new();

    operator::backup_create(&config, &backup, &mut out).unwrap();
    std::fs::create_dir(&restored).unwrap();
    operator::restore(&backup, &restored, false, &mut out).unwrap();

    let meta = MetaStore::open_existing(restored.join("velodex.redb")).unwrap();
    assert_eq!(meta.list_projects("local").unwrap(), vec!["Flask"]);
    assert_eq!(meta.list_upload_entries("local", "flask").unwrap().len(), 1);
    let blobs = BlobStore::new(restored.join("blobs"));
    assert_eq!(blobs.read(&content_digest).unwrap(), b"wheel bytes");
    assert_eq!(blobs.read(&metadata_digest).unwrap(), b"metadata bytes");
}

#[test]
fn test_backup_verify_reports_ok_for_valid_backup() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();

    let mut out = Vec::new();
    operator::backup_verify(&backup, &mut out).unwrap();

    assert!(String::from_utf8(out).unwrap().contains("ok\n"));
}

#[test]
fn test_backup_create_rejects_existing_target_paths() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let file_target = root.path().join("file-backup");
    std::fs::write(&file_target, b"x").unwrap();

    let err = operator::backup_create(&config, &file_target, &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("exists and is not a directory"));

    let dir_target = root.path().join("dir-backup");
    std::fs::create_dir(&dir_target).unwrap();
    std::fs::write(dir_target.join("blocker"), b"x").unwrap();
    let err = operator::backup_create(&config, &dir_target, &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("is not empty"));
}

#[test]
fn test_backup_create_rejects_missing_source_blob() {
    let (_source, config, content_digest, _metadata_digest) = backup_fixture();
    std::fs::remove_file(BlobStore::new(config.data_dir.join("blobs")).path_for(&content_digest)).unwrap();
    let root = tempfile::tempdir().unwrap();

    let err = operator::backup_create(&config, &root.path().join("backup"), &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("referenced blob"));
    assert!(err.to_string().contains("is missing"));
}

#[test]
fn test_backup_create_rejects_tampered_source_blob() {
    let (_source, config, content_digest, _metadata_digest) = backup_fixture();
    std::fs::write(
        BlobStore::new(config.data_dir.join("blobs")).path_for(&content_digest),
        b"tampered",
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();

    let err = operator::backup_create(&config, &root.path().join("backup"), &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("hashed as"));
}

#[test]
fn test_backup_verify_reports_missing_blob() {
    let (_source, config, content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    std::fs::remove_file(backup.join(blob_relpath(&content_digest))).unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains(&format!("problem\tblob\t{}\tmissing", content_digest.as_str()))
    );
}

#[test]
fn test_backup_verify_reports_mismatched_blob() {
    let (_source, config, content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    std::fs::write(backup.join(blob_relpath(&content_digest)), b"tampered").unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(String::from_utf8(out).unwrap().contains("sha256 expected"));
}

#[test]
fn test_backup_verify_rejects_unsupported_manifest_format() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    mutate_manifest(&backup, |manifest| manifest["format"] = serde_json::json!(2));

    let err = operator::backup_verify(&backup, &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("unsupported backup format 2"));
}

#[test]
fn test_backup_verify_reports_missing_metadata_store() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    std::fs::remove_file(backup.join("metadata/velodex.redb")).unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("problem\tmetadata\tmetadata/velodex.redb\tmissing")
    );
}

#[test]
fn test_backup_verify_reports_missing_manifest_files() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    std::fs::remove_file(backup.join("config.toml")).unwrap();
    std::fs::remove_file(backup.join("blobs.tsv")).unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("backup verification failed"));
    assert!(text.contains("problem\tconfig\tconfig.toml\tmissing"));
    assert!(text.contains("problem\tblob-index\tblobs.tsv\tmissing"));
}

#[test]
fn test_backup_verify_reports_corrupt_metadata_store() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    std::fs::write(backup.join("metadata/velodex.redb"), b"not a redb database").unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("problem\tmetadata\tmetadata/velodex.redb")
    );
}

#[test]
fn test_backup_verify_reports_manifest_file_mismatch() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    std::fs::write(backup.join("config.toml"), b"tampered config").unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("backup verification failed"));
    assert!(text.contains("problem\tconfig\tconfig.toml\tsha256 expected"));
    assert!(text.contains("problem\tconfig\tconfig.toml\tsize expected"));
}

#[test]
fn test_backup_verify_reports_blob_index_problems() {
    let (_source, config, content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    std::fs::write(
        backup.join("blobs.tsv"),
        format!(
            "bad header\n\nbad-row\nbad\t1\tbad\n{digest}\tbad\t{path}\n{digest}\t11\twrong/path\n{digest}\t11\t{path}\n{digest}\t11\t{path}\n",
            digest = content_digest.as_str(),
            path = blob_relpath(&content_digest),
        ),
    )
    .unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    let text = String::from_utf8(out).unwrap();
    assert!(err.to_string().contains("backup verification failed"));
    assert!(text.contains("problem\tblob-index\theader\tinvalid header"));
    assert!(text.contains("problem\tblob-index\tline 3\tinvalid row"));
    assert!(text.contains("problem\tblob-index\tline 4\tinvalid digest"));
    assert!(text.contains(&format!(
        "problem\tblob-index\t{}\tinvalid size",
        content_digest.as_str()
    )));
    assert!(text.contains("invalid size"));
    assert!(text.contains("invalid path"));
    assert!(text.contains("duplicate digest"));
    assert!(text.contains("missing referenced digest"));
    assert!(text.contains("problem\tblob-index\tcount"));
    assert!(text.contains("problem\tblob-index\tbytes"));
}

#[test]
fn test_backup_verify_reports_blob_size_mismatch() {
    let (_source, config, content_digest, metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    std::fs::write(
        backup.join("blobs.tsv"),
        format!(
            "sha256\tsize_bytes\tpath\n{content}\t999\t{content_path}\n{metadata}\t14\t{metadata_path}\n",
            content = content_digest.as_str(),
            content_path = blob_relpath(&content_digest),
            metadata = metadata_digest.as_str(),
            metadata_path = blob_relpath(&metadata_digest),
        ),
    )
    .unwrap();

    let mut out = Vec::new();
    let err = operator::backup_verify(&backup, &mut out).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(String::from_utf8(out).unwrap().contains("size expected 999"));
}

#[test]
fn test_restore_refuses_non_empty_target_without_force() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    let restored = root.path().join("restored");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    std::fs::create_dir_all(&restored).unwrap();
    std::fs::write(restored.join("blocker"), b"x").unwrap();

    let err = operator::restore(&backup, &restored, false, &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("not empty"));

    operator::restore(&backup, &restored, true, &mut Vec::new()).unwrap();
    assert!(restored.join("velodex.redb").is_file());
    assert!(!restored.join("blocker").exists());
}

#[test]
fn test_restore_refuses_file_target_without_force() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    let restored = root.path().join("restored");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    std::fs::write(&restored, b"x").unwrap();

    let err = operator::restore(&backup, &restored, false, &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("exists and is not a directory"));

    operator::restore(&backup, &restored, true, &mut Vec::new()).unwrap();
    assert!(restored.join("velodex.redb").is_file());
}

#[test]
fn test_restore_refuses_backup_with_verification_errors() {
    let (_source, config, content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    std::fs::remove_file(backup.join(blob_relpath(&content_digest))).unwrap();

    let err = operator::restore(&backup, &root.path().join("restored"), false, &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("backup verification failed"));
    assert!(err.to_string().contains("problem\tblob"));
}

#[test]
fn test_restore_warns_when_config_data_dir_changes() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    let restored = root.path().join("restored");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();

    let mut out = Vec::new();
    operator::restore(&backup, &restored, false, &mut out).unwrap();

    assert!(String::from_utf8(out).unwrap().contains("warning\tconfig\tdata_dir"));
}

#[test]
fn test_restore_skips_config_warning_when_data_dir_matches() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();

    let mut out = Vec::new();
    let err = operator::restore(&backup, &config.data_dir, false, &mut out).unwrap_err();

    assert!(err.to_string().contains("not empty"));
    assert!(!String::from_utf8(out).unwrap().contains("warning\tconfig\tdata_dir"));
}

#[test]
fn test_backup_create_snapshots_log_variants() {
    for (format, sink, expected) in [
        (LogFormat::Json, LogSink::File, "format = \"json\"\nsink = \"file\""),
        (
            LogFormat::Pretty,
            LogSink::Journald,
            "format = \"pretty\"\nsink = \"journald\"",
        ),
        (
            LogFormat::Pretty,
            LogSink::Syslog,
            "format = \"pretty\"\nsink = \"syslog\"",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        std::fs::create_dir(&data_dir).unwrap();
        drop(MetaStore::open(data_dir.join("velodex.redb")).unwrap());
        let backup = root.path().join("backup");

        operator::backup_create(
            &Config {
                data_dir,
                log: LogConfig {
                    format,
                    sink,
                    file: Some(root.path().join("velodex.log")),
                    ..LogConfig::default()
                },
                ..Config::default()
            },
            &backup,
            &mut Vec::new(),
        )
        .unwrap();

        assert!(
            std::fs::read_to_string(backup.join("config.toml"))
                .unwrap()
                .contains(expected)
        );
    }
}

#[test]
fn test_import_dir_validates_and_reports_files() {
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    std::fs::create_dir(&import).unwrap();
    std::fs::write(
        import.join("Flask-1.0-py3-none-any.whl"),
        wheel("Flask", "1.0", ">=3.8"),
    )
    .unwrap();
    std::fs::write(import.join("Demo-2.0.tar.gz"), sdist("Demo", "2.0")).unwrap();
    std::fs::write(import.join("Broken-1.0-py3-none-any.whl"), b"not a wheel").unwrap();
    std::fs::write(import.join("notes.txt"), b"skip").unwrap();
    let config = Config {
        data_dir: root.path().join("data"),
        ..Config::default()
    };

    let mut out = Vec::new();
    operator::import_dir(&config, "root/pypi", &import, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("imported\tDemo-2.0.tar.gz\tdemo\t2.0\tstored"));
    assert!(text.contains("imported\tFlask-1.0-py3-none-any.whl\tflask\t1.0\tstored"));
    assert!(text.contains("rejected\tBroken-1.0-py3-none-any.whl\tbroken\t1.0\tinvalid content"));
    assert!(text.contains("skipped\tnotes.txt\t\t\tunsupported file type"));
    assert!(text.contains("summary\t\t\t\timported=2 skipped=1 rejected=1"));

    let meta = MetaStore::open_existing(config.data_dir.join("velodex.redb")).unwrap();
    assert_eq!(meta.list_upload_entries("local", "demo").unwrap().len(), 1);
    assert_eq!(meta.list_upload_entries("local", "flask").unwrap().len(), 1);
}

#[test]
fn test_import_dir_reports_duplicate_nested_and_invalid_files() {
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    let nested = import.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("Flask-1.0-py3-none-any.whl"),
        wheel("Flask", "1.0", ">=3.8"),
    )
    .unwrap();
    std::fs::write(
        import.join("Flask-1.0-py3-none-any.whl"),
        wheel("Flask", "1.0", ">=3.8"),
    )
    .unwrap();
    std::fs::write(import.join("bad.whl"), b"not a valid wheel").unwrap();
    std::fs::write(import.join("Legacy-1.0-py3-none-any.egg"), b"egg").unwrap();
    let config = Config {
        data_dir: root.path().join("data"),
        ..Config::default()
    };

    let mut out = Vec::new();
    operator::import_dir(&config, "root/pypi", &import, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap().replace('\\', "/");
    assert!(text.contains("imported\tFlask-1.0-py3-none-any.whl\tflask\t1.0\tstored"));
    assert!(text.contains("skipped\tnested/Flask-1.0-py3-none-any.whl\tflask\t1.0\talready present"));
    assert!(
        text.contains("rejected\tbad.whl\t\t\tinvalid distribution filename"),
        "{text}"
    );
    assert!(text.contains("invalid distribution filename"), "{text}");
    assert!(text.contains("skipped\tLegacy-1.0-py3-none-any.egg\t\t\tunsupported file type"));
}

#[test]
fn test_import_dir_accepts_local_repository_route() {
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    std::fs::create_dir(&import).unwrap();
    std::fs::write(import.join("Demo-2.0.tar.gz"), sdist("Demo", "2.0")).unwrap();
    let config = Config {
        data_dir: root.path().join("data"),
        ..Config::default()
    };

    let mut out = Vec::new();
    operator::import_dir(&config, "local", &import, &mut out).unwrap();

    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("imported\tDemo-2.0.tar.gz\tdemo\t2.0\tstored")
    );
}

#[test]
fn test_import_dir_rejects_existing_filename_with_different_content() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    std::fs::create_dir(&import).unwrap();
    std::fs::write(
        import.join("Flask-1.0-py3-none-any.whl"),
        wheel("Flask", "1.0", ">=3.8"),
    )
    .unwrap();

    let mut out = Vec::new();
    operator::import_dir(&config, "root/pypi", &import, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("rejected\tFlask-1.0-py3-none-any.whl\tflask\t1.0"));
    assert!(text.contains("file already exists with different content"));
}

#[test]
fn test_import_dir_reports_metadata_validation_reasons() {
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    std::fs::create_dir(&import).unwrap();
    std::fs::write(
        import.join("InvalidPython-1.0-py3-none-any.whl"),
        wheel("InvalidPython", "1.0", "not a specifier"),
    )
    .unwrap();
    std::fs::write(
        import.join("NameMismatch-1.0-py3-none-any.whl"),
        wheel_with_identity("NameMismatch", "1.0", "Other", "1.0", ">=3.8"),
    )
    .unwrap();
    std::fs::write(
        import.join("Utf8-1.0-py3-none-any.whl"),
        wheel_with_metadata("Utf8", "1.0", b"\xff"),
    )
    .unwrap();
    std::fs::write(
        import.join("VersionMismatch-1.0-py3-none-any.whl"),
        wheel_with_identity("VersionMismatch", "1.0", "VersionMismatch", "2.0", ">=3.8"),
    )
    .unwrap();
    let config = Config {
        data_dir: root.path().join("data"),
        ..Config::default()
    };

    let mut out = Vec::new();
    operator::import_dir(&config, "root/pypi", &import, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("InvalidPython-1.0-py3-none-any.whl\tinvalidpython\t1.0\tinvalid Requires-Python"));
    assert!(text.contains("NameMismatch-1.0-py3-none-any.whl\tnamemismatch\t1.0\tmetadata name"));
    assert!(text.contains("Utf8-1.0-py3-none-any.whl\tutf8\t1.0\tmetadata is not UTF-8"));
    assert!(text.contains("VersionMismatch-1.0-py3-none-any.whl\tversionmismatch\t1.0\tmetadata version"));
}

#[test]
fn test_import_dir_rejects_unusable_repositories_and_paths() {
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    std::fs::create_dir(&import).unwrap();
    let mirror_config = Config {
        data_dir: root.path().join("mirror-data"),
        indexes: vec![IndexConfig {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            policy: velodex_policy::PolicyConfig::default(),
            webhooks: Vec::new(),
            ecosystem: velodex_format::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                upstream: "https://pypi.org/simple/".to_owned(),
                username: None,
                password: None,
                token: None,
                upstream_concurrency: velodex_http::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
                offline: false,
                prefetch: Box::default(),
            },
        }],
        ..Config::default()
    };
    let overlay_config = Config {
        data_dir: root.path().join("overlay-data"),
        indexes: vec![IndexConfig {
            name: "overlay".to_owned(),
            route: "overlay".to_owned(),
            policy: velodex_policy::PolicyConfig::default(),
            webhooks: Vec::new(),
            ecosystem: velodex_format::Ecosystem::Pypi,
            kind: IndexKind::Virtual {
                layers: Vec::new(),
                upload: None,
            },
        }],
        ..Config::default()
    };

    assert!(
        operator::import_dir(
            &Config::default(),
            "root/pypi",
            root.path().join("missing").as_path(),
            &mut Vec::new()
        )
        .is_err()
    );
    assert!(
        operator::import_dir(&mirror_config, "pypi", &import, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("read-only")
    );
    assert!(
        operator::import_dir(&overlay_config, "overlay", &import, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("no hosted upload target")
    );
    assert!(
        operator::import_dir(&overlay_config, "missing", &import, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("unknown index")
    );
}

fn backup_fixture() -> (tempfile::TempDir, Config, Digest, Digest) {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    let blobs = BlobStore::new(data_dir.join("blobs"));
    let content_digest = blobs.write(b"wheel bytes").unwrap();
    let metadata_digest = blobs.write(b"metadata bytes").unwrap();
    let meta = MetaStore::open(data_dir.join("velodex.redb")).unwrap();
    meta.put_upload(
        "local",
        "flask",
        "Flask-1.0-py3-none-any.whl",
        &uploaded_record_json(&content_digest, &metadata_digest),
    )
    .unwrap();
    meta.put_metadata(content_digest.as_str(), "uploaded", metadata_digest.as_str(), "local")
        .unwrap();
    meta.put_project("local", "flask", "Flask").unwrap();
    drop(meta);
    (
        dir,
        Config {
            data_dir,
            ..Config::default()
        },
        content_digest,
        metadata_digest,
    )
}

fn mutate_manifest(backup: &std::path::Path, mutate: impl FnOnce(&mut serde_json::Value)) {
    let path = backup.join("manifest.json");
    let mut manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    mutate(&mut manifest);
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

fn uploaded_record_json(content_digest: &Digest, metadata_digest: &Digest) -> Vec<u8> {
    to_json(&Uploaded {
        version: "1.0".to_owned(),
        file: File {
            filename: "Flask-1.0-py3-none-any.whl".to_owned(),
            url: format!(
                "/root/pypi/files/{}/Flask-1.0-py3-none-any.whl",
                content_digest.as_str()
            ),
            hashes: BTreeMap::from([("sha256".to_owned(), content_digest.as_str().to_owned())]),
            requires_python: None,
            size: Some(11),
            upload_time: Some("1970-01-01T00:00:00Z".to_owned()),
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Hashes(BTreeMap::from([(
                "sha256".to_owned(),
                metadata_digest.as_str().to_owned(),
            )])),
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
    })
    .into_bytes()
}

fn blob_relpath(digest: &Digest) -> String {
    let hex = digest.as_str();
    format!("blobs/sha256/{}/{}/{}", &hex[0..2], &hex[2..4], hex)
}

fn wheel(name: &str, version: &str, requires_python: &str) -> Vec<u8> {
    wheel_with_identity(name, version, name, version, requires_python)
}

fn wheel_with_identity(
    filename_name: &str,
    filename_version: &str,
    metadata_name: &str,
    metadata_version: &str,
    requires_python: &str,
) -> Vec<u8> {
    let metadata = format!(
        "Metadata-Version: 2.1\nName: {metadata_name}\nVersion: {metadata_version}\nRequires-Python: {requires_python}\n"
    );
    wheel_with_metadata(filename_name, filename_version, metadata.as_bytes())
}

fn wheel_with_metadata(name: &str, version: &str, metadata: &[u8]) -> Vec<u8> {
    let wheel = b"Wheel-Version: 1.0\nGenerator: velodex-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    let init = b"VALUE = 1\n";
    let dist_info = format!("{}-{version}.dist-info", name.to_ascii_lowercase());
    let record_path = format!("{dist_info}/RECORD");
    let entries = [
        (format!("{name}/__init__.py"), init.as_slice()),
        (format!("{dist_info}/METADATA"), metadata),
        (format!("{dist_info}/WHEEL"), wheel.as_slice()),
    ];
    let mut bytes = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
        let options = zip::write::SimpleFileOptions::default();
        for (path, content) in entries {
            zip.start_file(path, options).unwrap();
            zip.write_all(content).unwrap();
        }
        zip.start_file(&record_path, options).unwrap();
        zip.write_all(
            record(
                &[
                    (format!("{name}/__init__.py"), init.as_slice()),
                    (format!("{dist_info}/METADATA"), metadata),
                    (format!("{dist_info}/WHEEL"), wheel.as_slice()),
                ],
                &record_path,
            )
            .as_bytes(),
        )
        .unwrap();
        zip.finish().unwrap();
    }
    bytes
}

fn record(entries: &[(String, &[u8])], record_path: &str) -> String {
    let mut record = String::new();
    for (path, bytes) in entries {
        writeln!(
            record,
            "{path},sha256={},{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(bytes)),
            bytes.len()
        )
        .unwrap();
    }
    writeln!(record, "{record_path},,").unwrap();
    record
}

fn sdist(name: &str, version: &str) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let root = format!("{name}-{version}");
    append_tar_file(
        &mut archive,
        &format!("{root}/PKG-INFO"),
        format!("Metadata-Version: 2.2\nName: {name}\nVersion: {version}\n").as_bytes(),
    );
    append_tar_file(
        &mut archive,
        &format!("{root}/pyproject.toml"),
        b"[build-system]\nrequires = []\nbuild-backend = \"demo\"\n",
    );
    archive.into_inner().unwrap().finish().unwrap()
}

fn append_tar_file(archive: &mut tar::Builder<GzEncoder<Vec<u8>>>, path: &str, bytes: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive.append_data(&mut header, path, bytes).unwrap();
}
