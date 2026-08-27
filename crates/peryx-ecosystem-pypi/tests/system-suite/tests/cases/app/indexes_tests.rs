use rstest::rstest;

use super::*;
use crate::app;
use crate::cli::{IndexCommand, IndexListArgs, IndexShowArgs};
use crate::config::SecretSource;

fn index_list_command(ecosystem: Option<String>) -> IndexCommand {
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
    assert!(text.contains("root-pypi\troot/pypi\tpypi\tvirtual\tfalse"));
}

#[test]
fn test_index_list_filters_by_ecosystem() {
    let mut out = Vec::new();
    app::index(
        &Config::default(),
        &index_list_command(Some("pypi".to_owned())),
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
    assert!(text.contains("name\troot-pypi"));
    assert!(text.contains("route\troot/pypi"));
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
fn test_config_snippet_renders_pip_conf() {
    let text = app::config_snippet(
        &Config::default(),
        "root/pypi",
        "https://packages.example/cache",
        "pip.conf",
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
    config.indexes[1]
        .tokens
        .push(crate::support::writer_token(SecretSource::Literal("s3cret".to_owned())));

    let text = app::config_snippet(&config, "root/pypi", "https://packages.example", ".pypirc").unwrap();

    assert_eq!(
        text,
        "[distutils]\nindex-servers =\n    peryx\n\n[peryx]\nrepository = https://packages.example/root/pypi/\nusername = __token__\npassword = <upload-token>\n"
    );
}

#[test]
fn test_config_snippet_renders_uv_toml_with_upload_url() {
    let mut config = Config::default();
    config.indexes[1]
        .tokens
        .push(crate::support::writer_token(SecretSource::Literal("s3cret".to_owned())));

    let text = app::config_snippet(&config, "root/pypi", "https://packages.example", "uv.toml").unwrap();

    assert_eq!(
        text,
        "publish-url = \"https://packages.example/root/pypi/\"\n\n[[index]]\nname = \"peryx\"\nurl = \"https://packages.example/root/pypi/simple/\"\ndefault = true\n\n[pip]\nindex-url = \"https://packages.example/root/pypi/simple/\"\n"
    );
}

#[rstest]
#[case::pypirc_for_read_only_index("pypi", "https://packages.example", ".pypirc", "does not accept uploads")]
#[case::invalid_base_url("root/pypi", "not a url", "pip.conf", "base URL")]
#[case::unknown_index_route("missing", "https://packages.example", "pip.conf", "unknown index route")]
fn test_config_snippet_rejects(
    #[case] route: &str,
    #[case] base_url: &str,
    #[case] format: &str,
    #[case] expected: &str,
) {
    let err = app::config_snippet(&Config::default(), route, base_url, format).unwrap_err();
    assert!(err.to_string().contains(expected));
}

#[test]
fn test_config_snippet_rejects_invalid_index_configuration() {
    let mut config = Config::default();
    config.indexes[1].route = config.indexes[0].route.clone();
    let err = app::config_snippet(&config, "root/pypi", "https://packages.example", "pip.conf").unwrap_err();
    assert!(err.to_string().contains("duplicate index route"));
}
