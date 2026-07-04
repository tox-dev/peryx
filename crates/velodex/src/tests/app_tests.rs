use std::collections::BTreeMap;

use velodex_ecosystem_pypi::{CoreMetadata, File, Provenance, Yanked};
use velodex_http::upload::Uploaded;
use velodex_storage::blob::{BlobStore, Digest};
use velodex_storage::meta::{CachedIndex, MetaStore};

use crate::app::{self, init_data_dir};
use crate::cli::{
    CacheCommand, CacheListArgs, CachePurgeCommand, CachePurgeOrphanedBlobsArgs, CachePurgeProjectArgs,
    CacheRuntimeArgs, PolicyCommand, PolicyDryRunArgs, RuntimeArgs,
};
use crate::config::{Config, IndexKind};

struct FailImmediately;

impl std::io::Write for FailImmediately {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("write failed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct FailOnText {
    needle: &'static str,
    seen: String,
}

impl std::io::Write for FailOnText {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.seen.push_str(&String::from_utf8_lossy(buf));
        if self.seen.contains(self.needle) {
            return Err(std::io::Error::other("write failed"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn test_init_data_dir_creates_then_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("data");
    assert!(init_data_dir(&target).unwrap());
    assert!(!init_data_dir(&target).unwrap());
    assert!(target.is_dir());
}

#[test]
fn test_init_data_dir_errors_when_parent_is_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("blocker");
    std::fs::write(&file, "x").unwrap();
    assert!(init_data_dir(&file.join("sub")).is_err());
}

#[test]
fn test_init_creates_dir() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("d"),
        ..Config::default()
    };
    app::init(&config).unwrap();
    assert!(config.data_dir.is_dir());
}

#[test]
fn test_init_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("blocker");
    std::fs::write(&file, "x").unwrap();
    let config = Config {
        data_dir: file.join("sub"),
        ..Config::default()
    };
    assert!(app::init(&config).is_err());
}

#[test]
fn test_init_logs_both_branches_when_subscriber_enabled() {
    let subscriber = tracing_subscriber::fmt().with_writer(std::io::sink).finish();
    tracing::subscriber::with_default(subscriber, || {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: dir.path().join("d"),
            ..Config::default()
        };
        app::init(&config).unwrap(); // created
        app::init(&config).unwrap(); // already exists
    });
}

#[test]
fn test_config_snippet_renders_pip_conf() {
    let text = app::config_snippet(
        &Config::default(),
        "root/pypi",
        "https://packages.example/cache",
        velodex_http::discovery::SnippetKind::PipConf,
    )
    .unwrap();
    assert_eq!(
        text,
        "[global]\nindex-url = https://packages.example/cache/root/pypi/simple/\n"
    );
}

#[test]
fn test_config_snippet_redacts_upload_token() {
    let mut config = Config::default();
    let IndexKind::Local { upload_token, .. } = &mut config.indexes[1].kind else {
        panic!("expected local index");
    };
    *upload_token = Some("s3cret".to_owned());

    let text = app::config_snippet(
        &config,
        "root/pypi",
        "https://packages.example",
        velodex_http::discovery::SnippetKind::Pypirc,
    )
    .unwrap();

    assert_eq!(
        text,
        "[distutils]\nindex-servers =\n    velodex\n\n[velodex]\nrepository = https://packages.example/root/pypi/\nusername = __token__\npassword = <upload-token>\n"
    );
}

#[test]
fn test_config_snippet_renders_uv_toml_with_upload_url() {
    let mut config = Config::default();
    let IndexKind::Local { upload_token, .. } = &mut config.indexes[1].kind else {
        panic!("expected local index");
    };
    *upload_token = Some("s3cret".to_owned());

    let text = app::config_snippet(
        &config,
        "root/pypi",
        "https://packages.example",
        velodex_http::discovery::SnippetKind::UvToml,
    )
    .unwrap();

    assert_eq!(
        text,
        "publish-url = \"https://packages.example/root/pypi/\"\n\n[[index]]\nname = \"velodex\"\nurl = \"https://packages.example/root/pypi/simple/\"\ndefault = true\n\n[pip]\nindex-url = \"https://packages.example/root/pypi/simple/\"\n"
    );
}

#[test]
fn test_config_snippet_rejects_pypirc_for_read_only_index() {
    let err = app::config_snippet(
        &Config::default(),
        "pypi",
        "https://packages.example",
        velodex_http::discovery::SnippetKind::Pypirc,
    )
    .unwrap_err();
    assert!(err.to_string().contains("does not accept uploads"));
}

#[test]
fn test_config_snippet_rejects_invalid_base_url() {
    let err = app::config_snippet(
        &Config::default(),
        "root/pypi",
        "not a url",
        velodex_http::discovery::SnippetKind::PipConf,
    )
    .unwrap_err();
    assert!(err.to_string().contains("base URL"));
}

#[test]
fn test_config_snippet_rejects_unknown_index_route() {
    let err = app::config_snippet(
        &Config::default(),
        "missing",
        "https://packages.example",
        velodex_http::discovery::SnippetKind::PipConf,
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown index route"));
}

#[test]
fn test_config_snippet_rejects_invalid_index_configuration() {
    let mut config = Config::default();
    config.indexes[1].route = config.indexes[0].route.clone();
    let err = app::config_snippet(
        &config,
        "root/pypi",
        "https://packages.example",
        velodex_http::discovery::SnippetKind::PipConf,
    )
    .unwrap_err();
    assert!(err.to_string().contains("duplicate index route"));
}

#[test]
fn test_cache_list_reports_index_pages_and_blobs() {
    let (_dir, config, digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(&config, &cache_list_command(), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("kind\tindex\tproject\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n"));
    assert!(text.contains("index\tpypi\tflask\t\t"));
    assert!(text.contains(&format!("blob\t\t\t{}\t-\t-\t-\t7\t", digest.as_str())));
}

#[test]
fn test_cache_list_reports_write_errors() {
    let (_dir, config, _digest) = cache_fixture();
    let err = app::cache(&config, &cache_list_command(), &mut FailImmediately).unwrap_err();
    assert!(err.to_string().contains("write failed"));

    let mut out = FailOnText {
        needle: "index\tpypi\tflask",
        seen: String::new(),
    };
    let err = app::cache(&config, &cache_list_command(), &mut out).unwrap_err();
    assert!(err.to_string().contains("scan cached index pages"));
}

#[test]
fn test_cache_list_handles_unknown_index_keys() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    meta.put_index("loose", &cache_record(br#"{"files":[]}"#)).unwrap();
    meta.put_index("other/flask", &cache_record(br#"{"files":[]}"#))
        .unwrap();
    meta.put_index(
        "pypi/no-ttl",
        &CachedIndex {
            fresh_secs: None,
            ..cache_record(br#"{"files":[]}"#)
        },
    )
    .unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(&config, &cache_list_command(), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("index\tloose\t\t\t"));
    assert!(text.contains("index\tother\tflask\t\t"));
    assert!(text.contains("index\tpypi\tno-ttl\t\t"));
    assert!(text.contains("\t-\ttrue\t"));
}

#[test]
fn test_cache_missing_store_errors_with_path() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("missing"),
        ..Config::default()
    };
    let mut out = Vec::new();
    let err = app::cache(
        &config,
        &CacheCommand::Size(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap_err();
    assert!(err.to_string().contains("open metadata store"));
}

#[test]
fn test_cache_list_filters_skip_nonmatching_rows() {
    let (_dir, config, _digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::List(CacheListArgs {
            runtime: runtime_args(),
            index: Some("missing".to_owned()),
            project: None,
            digest: None,
            stale: false,
            min_age_secs: None,
            min_size_bytes: None,
        }),
        &mut out,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "kind\tindex\tproject\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n"
    );

    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::List(CacheListArgs {
            runtime: runtime_args(),
            index: None,
            project: Some("missing".to_owned()),
            digest: None,
            stale: false,
            min_age_secs: None,
            min_size_bytes: None,
        }),
        &mut out,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "kind\tindex\tproject\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n"
    );

    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::List(CacheListArgs {
            runtime: runtime_args(),
            index: None,
            project: None,
            digest: None,
            stale: false,
            min_age_secs: None,
            min_size_bytes: Some(1024),
        }),
        &mut out,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "kind\tindex\tproject\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n"
    );

    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::List(CacheListArgs {
            runtime: runtime_args(),
            index: None,
            project: None,
            digest: Some("0".repeat(64)),
            stale: false,
            min_age_secs: None,
            min_size_bytes: None,
        }),
        &mut out,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "kind\tindex\tproject\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n"
    );
}

#[test]
fn test_cache_list_min_age_filter_skips_future_pages() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    meta.put_index(
        "pypi/future",
        &CachedIndex {
            fetched_at_unix: i64::MAX,
            ..cache_record(br#"{"files":[]}"#)
        },
    )
    .unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::List(CacheListArgs {
            runtime: runtime_args(),
            index: None,
            project: None,
            digest: None,
            stale: false,
            min_age_secs: Some(1),
            min_size_bytes: None,
        }),
        &mut out,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "kind\tindex\tproject\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n"
    );
}

#[test]
fn test_cache_list_skips_invalid_blob_paths() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    write_invalid_blob_path(dir.path());
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(&config, &cache_list_command(), &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "kind\tindex\tproject\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n"
    );
}

#[test]
fn test_cache_size_reports_counts_and_stale_pages() {
    let (_dir, config, _digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Size(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("index_pages\t1\n"));
    assert!(text.contains("stale_index_pages\t1\n"));
    assert!(text.contains("blob_files\t1\n"));
    assert!(text.contains("blob_bytes\t7\n"));
    assert!(text.contains("file_url_records\t1\n"));
    assert!(text.contains("metadata_records\t1\n"));
    assert!(text.contains("project_records\t1\n"));
}

#[test]
fn test_cache_size_counts_invalid_blob_paths() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    write_invalid_blob_path(dir.path());
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Size(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap();
    assert!(String::from_utf8(out).unwrap().contains("invalid_blob_paths\t1\n"));
}

#[test]
fn test_cache_size_counts_uploads_and_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    meta.put_upload(
        "local",
        "pkg",
        "pkg-1.0.whl",
        &uploaded_record_json(&Digest::of(b"pkg")),
    )
    .unwrap();
    meta.put_override("local", "pkg", "pkg-1.0.whl", "hidden").unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Size(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("upload_records\t1\n"));
    assert!(text.contains("override_records\t1\n"));
}

#[test]
fn test_cache_fsck_reports_ok_for_valid_store() {
    let (_dir, config, _digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Fsck(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "ok\n");
}

#[test]
fn test_cache_fsck_reports_metadata_problems() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    meta.put_index(
        "pypi/bad",
        &CachedIndex {
            body: b"not json".to_vec(),
            ..cache_record(b"not json")
        },
    )
    .unwrap();
    meta.put_file_url("bad", "https://files.example/pkg.whl", "pypi")
        .unwrap();
    meta.put_metadata("bad", "https://files.example/pkg.whl.metadata", "also-bad", "pypi")
        .unwrap();
    meta.put_project("", "", "").unwrap();
    meta.put_upload("local", "pkg", "bad.whl", b"not json").unwrap();
    meta.put_upload("", "", "", &uploaded_record_json(&Digest::of(b"missing")))
        .unwrap();
    meta.put_upload(
        "local",
        "pkg",
        "pkg-1.0.whl",
        &uploaded_record_json(&Digest::of(b"missing")),
    )
    .unwrap();
    meta.put_override("", "", "", "bad").unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Fsck(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    for expected in [
        "metadata\tindex\tpypi/bad\tinvalid project detail\n",
        "metadata\tfile-url\tbad\tinvalid record\n",
        "metadata\tpep658\tbad\tinvalid record\n",
        "metadata\tproject\t/\tinvalid record\n",
        "metadata\tupload\tlocal/pkg/bad.whl\tinvalid record\n",
        "metadata\tupload\t//\tinvalid key\n",
        "metadata\tupload\tlocal/pkg/pkg-1.0.whl\tmissing blob ",
        "metadata\toverride\t//\tinvalid record\n",
        "problems\t8\n",
    ] {
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn test_cache_fsck_reports_missing_metadata_blob() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let digest = Digest::of(b"wheel");
    let metadata_digest = Digest::of(b"metadata");
    meta.put_upload(
        "local",
        "pkg",
        "pkg-1.0.whl",
        &uploaded_record_json_with_metadata(&digest, &metadata_digest),
    )
    .unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Fsck(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(&format!(
        "metadata\tupload\tlocal/pkg/pkg-1.0.whl\tmissing blob {}",
        digest.as_str()
    )));
    assert!(text.contains(&format!(
        "metadata\tupload\tlocal/pkg/pkg-1.0.whl\tmissing blob {}",
        metadata_digest.as_str()
    )));
}

#[test]
fn test_cache_fsck_accepts_valid_upload_and_override() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let digest = blobs.write(b"pkg").unwrap();
    meta.put_upload("local", "pkg", "pkg-1.0.whl", &uploaded_record_json(&digest))
        .unwrap();
    meta.put_override("local", "pkg", "pkg-1.0.whl", "hidden").unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Fsck(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "ok\n");
}

#[test]
fn test_cache_fsck_reports_blob_hash_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"expected");
    let path = blobs.path_for(&digest);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"tampered").unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Fsck(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!("blob\thash\t{}\tdigest mismatch\nproblems\t1\n", digest.as_str())
    );
}

#[cfg(unix)]
#[test]
fn test_cache_fsck_reports_blob_read_errors() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"blocked");
    let path = blobs.path_for(&digest);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"blocked").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Fsck(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(String::from_utf8(out).unwrap().contains("blob\tread\t"));
}

#[test]
fn test_cache_fsck_reports_corrupt_index_record() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("velodex.redb");
    MetaStore::open(&db_path).unwrap();
    raw_insert_bytes(&db_path, "simple_index", "pypi/corrupt", b"not json");
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Fsck(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("metadata\tindex\tpypi/corrupt\t")
    );
}

#[test]
fn test_cache_fsck_reports_invalid_blob_path() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    write_invalid_blob_path(dir.path());
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Fsck(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("invalid content-addressed path")
    );
}

#[test]
fn test_cache_fsck_reports_write_errors() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    write_invalid_blob_path(dir.path());
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = FailOnText {
        needle: "invalid content-addressed path",
        seen: String::new(),
    };
    let err = app::cache(
        &config,
        &CacheCommand::Fsck(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut out,
    )
    .unwrap_err();
    assert!(err.to_string().contains("scan blob files"));
}

#[test]
fn test_cache_purge_project_dry_run_keeps_records() {
    let (_dir, config, digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(&config, &purge_project_command(false), &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "action\ttarget\tindex\tproject\tindex_pages\tproject_records\tfile_url_records\tmetadata_records\n\
dry-run\tproject\tpypi\tflask\t1\t1\t1\t1\n"
    );
    let meta = MetaStore::open_existing(config.data_dir.join("velodex.redb")).unwrap();
    assert!(meta.get_index("pypi/flask").unwrap().is_some());
    assert!(meta.get_file_url(digest.as_str()).unwrap().is_some());
}

#[test]
fn test_cache_purge_project_missing_target_is_empty() {
    let (_dir, config, _digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Purge(CachePurgeCommand::Project(CachePurgeProjectArgs {
            runtime: runtime_args(),
            index: "pypi".to_owned(),
            project: "missing".to_owned(),
            yes: false,
        })),
        &mut out,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "action\ttarget\tindex\tproject\tindex_pages\tproject_records\tfile_url_records\tmetadata_records\n\
dry-run\tproject\tpypi\tmissing\t0\t0\t0\t0\n"
    );
}

#[test]
fn test_cache_purge_project_preserves_shared_and_uploaded_blobs() {
    let (_dir, config, digest) = cache_fixture();
    let meta = MetaStore::open_existing(config.data_dir.join("velodex.redb")).unwrap();
    meta.put_index(
        "pypi/other",
        &CachedIndex {
            body: format!(
                r#"{{"meta":{{"api-version":"1.1"}},"name":"other","versions":["1.0"],"files":[{{"filename":"other-1.0.whl","url":"https://files.example/other.whl","hashes":{{"sha256":"{}"}},"core-metadata":false,"yanked":false}}]}}"#,
                digest.as_str()
            )
            .into_bytes(),
            ..cache_record(b"")
        },
    )
    .unwrap();
    meta.put_upload(
        "local",
        "pkg",
        "pkg-1.0.whl",
        &uploaded_record_json(&Digest::of(b"uploaded")),
    )
    .unwrap();
    drop(meta);
    let mut out = Vec::new();
    app::cache(&config, &purge_project_command(false), &mut out).unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("dry-run\tproject\tpypi\tflask\t1\t1\t0\t0\n")
    );
}

#[test]
fn test_cache_purge_project_reports_corrupt_target_record() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("velodex.redb");
    MetaStore::open(&db_path).unwrap();
    raw_insert_bytes(&db_path, "simple_index", "pypi/flask", b"not json");
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_project_command(false), &mut out).unwrap_err();
    assert!(
        err.chain()
            .any(|cause| cause.to_string().contains("read cached project pypi/flask"))
    );
}

#[test]
fn test_cache_purge_project_reports_corrupt_shared_record() {
    let (_dir, config, _digest) = cache_fixture();
    raw_insert_bytes(
        &config.data_dir.join("velodex.redb"),
        "simple_index",
        "pypi/other",
        b"not json",
    );
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_project_command(false), &mut out).unwrap_err();
    assert!(err.to_string().contains("scan cached pages for shared digests"));
}

#[test]
fn test_cache_purge_project_reports_corrupt_upload_record() {
    let (_dir, config, _digest) = cache_fixture();
    raw_insert_bytes(
        &config.data_dir.join("velodex.redb"),
        "uploads",
        "local/pkg/bad.whl",
        b"not json",
    );
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_project_command(false), &mut out).unwrap_err();
    assert!(err.to_string().contains("scan upload records for shared digests"));
}

#[test]
fn test_cache_purge_project_rejects_invalid_cached_file_digest() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            body: br#"{"meta":{"api-version":"1.1"},"name":"flask","versions":["1.0"],"files":[{"filename":"flask-1.0.whl","url":"https://files.example/flask.whl","hashes":{"sha256":"bad"},"core-metadata":false,"yanked":false}]}"#.to_vec(),
            ..cache_record(b"")
        },
    )
    .unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_project_command(false), &mut out).unwrap_err();
    assert!(
        err.chain()
            .any(|cause| cause.to_string().contains("invalid sha256 digest"))
    );
}

#[test]
fn test_cache_purge_project_rejects_invalid_cached_metadata_digest() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let digest = Digest::of(b"wheel");
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            body: format!(
                r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","versions":["1.0"],"files":[{{"filename":"flask-1.0.whl","url":"https://files.example/flask.whl","hashes":{{"sha256":"{}"}},"core-metadata":{{"sha256":"bad"}},"yanked":false}}]}}"#,
                digest.as_str()
            )
            .into_bytes(),
            ..cache_record(b"")
        },
    )
    .unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_project_command(false), &mut out).unwrap_err();
    assert!(
        err.chain()
            .any(|cause| cause.to_string().contains("invalid metadata digest"))
    );
}

#[test]
fn test_cache_purge_project_ignores_files_without_sha256() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            body: br#"{"meta":{"api-version":"1.1"},"name":"flask","versions":["1.0"],"files":[{"filename":"flask-1.0.whl","url":"https://files.example/flask.whl","hashes":{},"core-metadata":false,"yanked":false}]}"#.to_vec(),
            ..cache_record(b"")
        },
    )
    .unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(&config, &purge_project_command(false), &mut out).unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("dry-run\tproject\tpypi\tflask\t1\t0\t0\t0\n")
    );
}

#[test]
fn test_cache_purge_project_reports_write_errors() {
    let (_dir, config, _digest) = cache_fixture();
    let mut out = FailOnText {
        needle: "dry-run",
        seen: String::new(),
    };
    let err = app::cache(&config, &purge_project_command(false), &mut out).unwrap_err();
    assert!(err.to_string().contains("write failed"));
}

#[test]
fn test_cache_purge_project_yes_removes_metadata_records() {
    let (_dir, config, digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(&config, &purge_project_command(true), &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "action\ttarget\tindex\tproject\tindex_pages\tproject_records\tfile_url_records\tmetadata_records\n\
removed\tproject\tpypi\tflask\t1\t1\t1\t1\n"
    );
    let meta = MetaStore::open_existing(config.data_dir.join("velodex.redb")).unwrap();
    assert!(meta.get_index("pypi/flask").unwrap().is_none());
    assert!(meta.get_file_url(digest.as_str()).unwrap().is_none());
    assert!(meta.get_metadata(digest.as_str()).unwrap().is_none());
    assert!(meta.list_projects("pypi").unwrap().is_empty());
}

#[test]
fn test_cache_purge_orphaned_blobs_rejects_invalid_references() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    meta.put_file_url("bad", "https://files.example/pkg.whl", "pypi")
        .unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap_err();
    assert!(err.to_string().contains("scan file URL references"));
}

#[test]
fn test_cache_purge_orphaned_blobs_rejects_invalid_metadata_references() {
    let valid = Digest::of(b"valid");
    for (wheel, metadata, raw) in [
        ("bad".to_owned(), valid.as_str().to_owned(), None),
        (valid.as_str().to_owned(), "bad".to_owned(), None),
        (
            valid.as_str().to_owned(),
            valid.as_str().to_owned(),
            Some("missing-parts"),
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("velodex.redb");
        let meta = MetaStore::open(&db_path).unwrap();
        if let Some(raw) = raw {
            drop(meta);
            raw_insert_str(&db_path, "metadata", &wheel, raw);
        } else {
            meta.put_metadata(&wheel, "https://files.example/pkg.whl.metadata", &metadata, "pypi")
                .unwrap();
            drop(meta);
        }
        let config = Config {
            data_dir: dir.path().to_path_buf(),
            ..Config::default()
        };
        let mut out = Vec::new();
        let err = app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap_err();
        assert!(err.to_string().contains("scan PEP 658 references"));
    }
}

#[test]
fn test_cache_purge_orphaned_blobs_rejects_invalid_upload_references() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    meta.put_upload("local", "pkg", "bad.whl", b"not json").unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap_err();
    assert!(err.to_string().contains("scan upload references"));
}

#[test]
fn test_cache_purge_orphaned_blobs_keeps_referenced_upload_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let digest = blobs.write(b"pkg").unwrap();
    meta.put_upload("local", "pkg", "pkg-1.0.whl", &uploaded_record_json(&digest))
        .unwrap();
    drop(meta);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("summary\tdry-run\torphaned-blobs\t0\t0\n")
    );
}

#[test]
fn test_cache_purge_orphaned_blobs_skips_invalid_blob_paths() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    write_invalid_blob_path(dir.path());
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let mut out = Vec::new();
    app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("summary\tdry-run\torphaned-blobs\t0\t0\n")
    );
}

#[test]
fn test_cache_purge_orphaned_blobs_reports_write_errors() {
    let (_dir, config, _digest) = cache_fixture();
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    blobs.write(b"orphan").unwrap();
    for needle in ["orphaned-blob", "summary"] {
        let mut out = FailOnText {
            needle,
            seen: String::new(),
        };
        let err = app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap_err();
        assert!(err.to_string().contains("scan orphaned blob files") || err.to_string().contains("write failed"));
    }
}

#[test]
fn test_cache_purge_orphaned_blobs_dry_run_keeps_blob() {
    let (_dir, config, _digest) = cache_fixture();
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    let orphan = blobs.write(b"orphan").unwrap();
    let mut out = Vec::new();
    app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(&format!("dry-run\torphaned-blob\t{}\t6\t", orphan.as_str())));
    assert!(text.contains("summary\tdry-run\torphaned-blobs\t1\t6\n"));
    assert!(blobs.exists(&orphan));
}

#[test]
fn test_cache_purge_orphaned_blobs_yes_removes_blob() {
    let (_dir, config, _digest) = cache_fixture();
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    let orphan = blobs.write(b"orphan").unwrap();
    let mut out = Vec::new();
    app::cache(&config, &purge_orphaned_blobs_command(true), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(&format!("removed\torphaned-blob\t{}\t6\t", orphan.as_str())));
    assert!(text.contains("summary\tremoved\torphaned-blobs\t1\t6\n"));
    assert!(!blobs.exists(&orphan));
}

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
    MetaStore::open(config.data_dir.join("velodex.redb"))
        .unwrap()
        .put_upload("local", "pkg", "pkg-1.0.whl", &uploaded_record_json(&digest))
        .unwrap();
    config.indexes[1].policy.max_file_size_bytes = Some(2);
    let mut out = Vec::new();

    app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: Some("local".to_owned()),
            project: Some("pkg".to_owned()),
        }),
        &mut out,
    )
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("upload\tlocal\tpkg\tpkg-1.0.whl\t\tmax-file-size\tsize\tfile size 3 exceeds limit 2\n"),
        "{text}"
    );
}

#[test]
fn test_policy_dry_run_skips_allowed_upload() {
    let (_dir, config, digest) = cache_fixture();
    MetaStore::open(config.data_dir.join("velodex.redb"))
        .unwrap()
        .put_upload("local", "pkg", "pkg-1.0.whl", &uploaded_record_json(&digest))
        .unwrap();
    let mut out = Vec::new();

    app::policy(
        &config,
        &PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: runtime_args(),
            index: Some("local".to_owned()),
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
    let db_path = config.data_dir.join("velodex.redb");
    raw_insert_bytes(&db_path, "uploads", "loose", b"not json");
    raw_insert_bytes(
        &db_path,
        "uploads",
        "foreign/pkg/pkg-1.0.whl",
        &uploaded_record_json(&digest),
    );
    raw_insert_bytes(&db_path, "uploads", "local/pkg/pkg-1.0.whl", b"not json");
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
    MetaStore::open(config.data_dir.join("velodex.redb"))
        .unwrap()
        .put_upload("local", "pkg", "pkg-1.0.whl", &uploaded_record_json(&digest))
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
            index: Some("local".to_owned()),
            project: Some("pkg".to_owned()),
        }),
        &mut out,
    )
    .unwrap_err();

    assert!(err.to_string().contains("scan upload records"));
}

fn cache_record(body: &[u8]) -> CachedIndex {
    CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 0,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: Some(1),
        body: body.to_vec(),
    }
}

fn cache_fixture() -> (tempfile::TempDir, Config, Digest) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let digest = BlobStore::new(dir.path().join("blobs")).write(b"payload").unwrap();
    let metadata_digest = Digest::of(b"metadata");
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            body: format!(
                r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","versions":["1.0"],"files":[{{"filename":"flask-1.0.whl","url":"https://files.example/flask.whl","hashes":{{"sha256":"{}"}},"core-metadata":{{"sha256":"{}"}},"yanked":false}}]}}"#,
                digest.as_str(),
                metadata_digest.as_str()
            )
            .into_bytes(),
            ..cache_record(b"")
        },
    )
    .unwrap();
    meta.put_project("pypi", "flask", "Flask").unwrap();
    meta.put_file_url(digest.as_str(), "https://files.example/flask.whl", "pypi")
        .unwrap();
    meta.put_metadata(
        digest.as_str(),
        "https://files.example/flask.whl.metadata",
        metadata_digest.as_str(),
        "pypi",
    )
    .unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    (dir, config, digest)
}

fn uploaded_record_json(digest: &Digest) -> Vec<u8> {
    let mut hashes = BTreeMap::new();
    hashes.insert("sha256".to_owned(), digest.as_str().to_owned());
    serde_json::to_vec(&Uploaded {
        version: "1.0".to_owned(),
        file: File {
            filename: "pkg-1.0.whl".to_owned(),
            url: format!("http://localhost/files/{}/pkg-1.0.whl", digest.as_str()),
            hashes,
            requires_python: None,
            size: Some(3),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
    })
    .unwrap()
}

fn uploaded_record_json_with_metadata(digest: &Digest, metadata_digest: &Digest) -> Vec<u8> {
    let mut metadata_hashes = BTreeMap::new();
    metadata_hashes.insert("sha256".to_owned(), metadata_digest.as_str().to_owned());
    let mut upload: Uploaded = serde_json::from_slice(&uploaded_record_json(digest)).unwrap();
    upload.file.core_metadata = CoreMetadata::Hashes(metadata_hashes);
    serde_json::to_vec(&upload).unwrap()
}

fn write_invalid_blob_path(root: &std::path::Path) {
    let path = root.join("blobs/sha256/aa/bb/not-a-digest");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"x").unwrap();
}

fn raw_insert_bytes(path: &std::path::Path, table: &'static str, key: &str, value: &[u8]) {
    let db = redb::Database::open(path).unwrap();
    let table = redb::TableDefinition::<&str, &[u8]>::new(table);
    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(table).unwrap();
        table.insert(key, value).unwrap();
    }
    txn.commit().unwrap();
}

fn raw_insert_str(path: &std::path::Path, table: &'static str, key: &str, value: &str) {
    let db = redb::Database::open(path).unwrap();
    let table = redb::TableDefinition::<&str, &str>::new(table);
    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(table).unwrap();
        table.insert(key, value).unwrap();
    }
    txn.commit().unwrap();
}

fn cache_list_command() -> CacheCommand {
    CacheCommand::List(CacheListArgs {
        runtime: runtime_args(),
        index: None,
        project: None,
        digest: None,
        stale: false,
        min_age_secs: None,
        min_size_bytes: None,
    })
}

fn purge_project_command(yes: bool) -> CacheCommand {
    CacheCommand::Purge(CachePurgeCommand::Project(CachePurgeProjectArgs {
        runtime: runtime_args(),
        index: "pypi".to_owned(),
        project: "Flask".to_owned(),
        yes,
    }))
}

fn purge_orphaned_blobs_command(yes: bool) -> CacheCommand {
    CacheCommand::Purge(CachePurgeCommand::OrphanedBlobs(CachePurgeOrphanedBlobsArgs {
        runtime: runtime_args(),
        yes,
    }))
}

fn runtime_args() -> RuntimeArgs {
    RuntimeArgs {
        config: None,
        host: None,
        port: None,
        data_dir: None,
        offline: false,
        log_level: None,
        verbose: 0,
        log_format: None,
        log_sink: None,
        log_file: None,
    }
}
