use peryx_ecosystem_pypi::discovery::SnippetKind;
use rstest::rstest;

use super::*;
use crate::app::{self, init_data_dir};
use crate::cli::{EcosystemArg, IndexCommand, IndexListArgs, IndexShowArgs};
use crate::config::IndexKind;

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

fn index_list_command(ecosystem: Option<EcosystemArg>) -> IndexCommand {
    IndexCommand::List(IndexListArgs {
        runtime: RuntimeArgs::default(),
        ecosystem,
    })
}

#[test]
fn test_index_list_prints_every_configured_index() {
    let mut out = Vec::new();
    app::index(&Config::default(), &index_list_command(None), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("name\troute\tecosystem\tkind\tuploads\n"));
    assert!(text.contains("pypi\tpypi\tpypi\tcached\tfalse"));
    assert!(text.contains("hosted\thosted\tpypi\thosted\tfalse"));
    assert!(text.contains("root/pypi\troot/pypi\tpypi\tvirtual\tfalse"));
}

#[test]
fn test_index_list_filters_by_ecosystem() {
    let mut out = Vec::new();
    app::index(
        &Config::default(),
        &index_list_command(Some(EcosystemArg::Pypi)),
        &mut out,
    )
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text.lines().filter(|line| line.contains("\tpypi\t")).count(), 3);
}

#[test]
fn test_index_show_prints_virtual_detail() {
    let command = IndexCommand::Show(IndexShowArgs {
        runtime: RuntimeArgs::default(),
        index: "root/pypi".to_owned(),
    });
    let mut out = Vec::new();
    app::index(&Config::default(), &command, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("kind\tvirtual"));
    assert!(text.contains("layers\thosted, pypi"));
    assert!(text.contains("upload_to\thosted"));
}

#[test]
fn test_index_show_prints_cached_upstream() {
    let command = IndexCommand::Show(IndexShowArgs {
        runtime: RuntimeArgs::default(),
        index: "pypi".to_owned(),
    });
    let mut out = Vec::new();
    app::index(&Config::default(), &command, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("kind\tcached"));
    assert!(text.contains("upstream\thttps://pypi.org/simple/"));
    assert!(text.contains("offline\tfalse"));
}

#[test]
fn test_index_show_rejects_unknown_index() {
    let command = IndexCommand::Show(IndexShowArgs {
        runtime: RuntimeArgs::default(),
        index: "nope".to_owned(),
    });
    let err = app::index(&Config::default(), &command, &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("unknown index \"nope\""));
}

#[test]
fn test_index_list_propagates_header_write_failure() {
    let err = app::index(&Config::default(), &index_list_command(None), &mut FailImmediately).unwrap_err();
    assert!(err.to_string().contains("write failed"));
}

#[test]
fn test_index_list_propagates_row_write_failure() {
    let err = app::index(
        &Config::default(),
        &index_list_command(None),
        &mut FailOnText {
            needle: "cached",
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("write failed"));
}

#[test]
fn test_index_show_propagates_write_failure() {
    let command = IndexCommand::Show(IndexShowArgs {
        runtime: RuntimeArgs::default(),
        index: "root/pypi".to_owned(),
    });
    let err = app::index(
        &Config::default(),
        &command,
        &mut FailOnText {
            needle: "layers",
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("write failed"));
}

#[test]
fn test_config_snippet_renders_pip_conf() {
    let text = app::config_snippet(
        &Config::default(),
        "root/pypi",
        "https://packages.example/cache",
        peryx_ecosystem_pypi::discovery::SnippetKind::PipConf,
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
    let IndexKind::Hosted { upload_token, .. } = &mut config.indexes[1].kind else {
        panic!("expected hosted index");
    };
    *upload_token = Some("s3cret".to_owned());

    let text = app::config_snippet(
        &config,
        "root/pypi",
        "https://packages.example",
        peryx_ecosystem_pypi::discovery::SnippetKind::Pypirc,
    )
    .unwrap();

    assert_eq!(
        text,
        "[distutils]\nindex-servers =\n    peryx\n\n[peryx]\nrepository = https://packages.example/root/pypi/\nusername = __token__\npassword = <upload-token>\n"
    );
}

#[test]
fn test_config_snippet_renders_uv_toml_with_upload_url() {
    let mut config = Config::default();
    let IndexKind::Hosted { upload_token, .. } = &mut config.indexes[1].kind else {
        panic!("expected hosted index");
    };
    *upload_token = Some("s3cret".to_owned());

    let text = app::config_snippet(
        &config,
        "root/pypi",
        "https://packages.example",
        peryx_ecosystem_pypi::discovery::SnippetKind::UvToml,
    )
    .unwrap();

    assert_eq!(
        text,
        "publish-url = \"https://packages.example/root/pypi/\"\n\n[[index]]\nname = \"peryx\"\nurl = \"https://packages.example/root/pypi/simple/\"\ndefault = true\n\n[pip]\nindex-url = \"https://packages.example/root/pypi/simple/\"\n"
    );
}

#[rstest]
#[case::pypirc_for_read_only_index("pypi", "https://packages.example", SnippetKind::Pypirc, "does not accept uploads")]
#[case::invalid_base_url("root/pypi", "not a url", SnippetKind::PipConf, "base URL")]
#[case::unknown_index_route("missing", "https://packages.example", SnippetKind::PipConf, "unknown index route")]
fn test_config_snippet_rejects(
    #[case] route: &str,
    #[case] base_url: &str,
    #[case] kind: SnippetKind,
    #[case] expected: &str,
) {
    let err = app::config_snippet(&Config::default(), route, base_url, kind).unwrap_err();
    assert!(err.to_string().contains(expected));
}

#[test]
fn test_config_snippet_rejects_invalid_index_configuration() {
    let mut config = Config::default();
    config.indexes[1].route = config.indexes[0].route.clone();
    let err = app::config_snippet(
        &config,
        "root/pypi",
        "https://packages.example",
        peryx_ecosystem_pypi::discovery::SnippetKind::PipConf,
    )
    .unwrap_err();
    assert!(err.to_string().contains("duplicate index route"));
}
