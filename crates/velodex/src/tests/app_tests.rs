use crate::app::{self, init_data_dir};
use crate::config::{Config, IndexKind};

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
