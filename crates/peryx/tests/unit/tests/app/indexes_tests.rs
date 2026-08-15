use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _};
use std::sync::Mutex;

use peryx_plugin_registry::PluginRegistry;
use rstest::rstest;

use super::*;
use crate::app::tests::{bounded_output, plugins, runtime_args};
use crate::cli::{IndexListArgs, IndexShowArgs};
use crate::config::{IndexConfig, IndexKind, UpstreamConfig, UpstreamRoutingConfig, UpstreamTlsConfig};

#[test]
fn test_init_reports_created_and_existing_directories() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("data"),
        ..Config::default()
    };
    let mut log = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(Mutex::new(log.try_clone().unwrap()))
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        init(&config).unwrap();
        init(&config).unwrap();
    });

    let mut text = String::new();
    log.rewind().unwrap();
    log.read_to_string(&mut text).unwrap();
    assert!(text.contains("initialized data directory"), "{text}");
    assert!(text.contains("data directory already exists"), "{text}");
}

#[test]
fn test_index_lists_and_filters_configured_indexes() {
    let mut output = Vec::new();
    let plugins = plugins();
    let config = Config::with_plugins(&plugins);
    index_with_plugins(&config, &plugins, &list_command(None), &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.starts_with("name\troute\tecosystem\tkind\tuploads\n"));
    assert!(output.contains("main\tmain\tcore\thosted\tfalse\n"), "{output}");

    let mut filtered = Vec::new();
    index_with_plugins(&config, &plugins, &list_command(Some("core".to_owned())), &mut filtered).unwrap();
    assert!(
        String::from_utf8(filtered)
            .unwrap()
            .lines()
            .skip(1)
            .all(|line| line.contains("\tcore\t"))
    );
}

#[test]
fn test_index_list_filter_can_match_no_indexes() {
    let plugins = plugins();
    let mut output = Vec::new();

    index_with_plugins(
        &Config::with_plugins(&plugins),
        &plugins,
        &list_command(Some("missing".to_owned())),
        &mut output,
    )
    .unwrap();

    assert_eq!(
        String::from_utf8(output).unwrap(),
        "name\troute\tecosystem\tkind\tuploads\n"
    );
}

#[rstest]
#[case::hosted("main", &["kind\thosted", "uploads\tfalse"])]
fn test_index_show_reports_index_details(#[case] selector: &str, #[case] expected: &[&str]) {
    let plugins = plugins();
    let mut output = Vec::new();

    index_with_plugins(
        &Config::with_plugins(&plugins),
        &plugins,
        &show_command(selector),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(expected.iter().all(|line| output.contains(line)), "{output}");
}

#[rstest]
#[case::cached("cache", &["kind\tcached", "upstream\thttps://upstream.example/packages/", "offline\tfalse"])]
#[case::virtual_index("combined", &["kind\tvirtual", "layers\tcache, main", "upload_to\tmain"])]
fn test_index_show_reports_optional_details(#[case] selector: &str, #[case] expected: &[&str]) {
    let plugins = plugins();
    let mut output = Vec::new();

    index_with_plugins(
        &detailed_config(&plugins),
        &plugins,
        &show_command(selector),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(expected.iter().all(|line| output.contains(line)), "{output}");
}

#[test]
fn test_index_show_rejects_an_unknown_index() {
    let plugins = plugins();
    let error = index_with_plugins(
        &Config::with_plugins(&plugins),
        &plugins,
        &show_command("missing"),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert_eq!(error.to_string(), "unknown index \"missing\"");
}

#[rstest]
#[case::list_header(list_command(None), 0)]
#[case::list_row(list_command(None), "name\troute\tecosystem\tkind\tuploads\n".len())]
#[case::show_name(show_command("main"), 0)]
fn test_index_commands_propagate_output_failures(#[case] command: IndexCommand, #[case] capacity: usize) {
    let plugins = plugins();
    let error = index_with_plugins(
        &Config::with_plugins(&plugins),
        &plugins,
        &command,
        &mut bounded_output(capacity),
    )
    .unwrap_err();

    assert!(error.to_string().contains("failed to write whole buffer"));
}

#[test]
fn test_index_rejects_invalid_topology() {
    let plugins = plugins();
    let mut config = Config::with_plugins(&plugins);
    config.indexes.push(config.indexes[0].clone());

    let error = index_with_plugins(&config, &plugins, &list_command(None), &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("duplicate"), "{error:#}");
}

#[test]
fn test_index_rejects_an_uninstalled_ecosystem_before_dispatch() {
    let plugins = plugins();
    let error = index_with_plugins(
        &Config::with_plugins(&plugins),
        &crate::tests::support::plugins_without_retention(),
        &list_command(None),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert_eq!(
        format!("{error:#}"),
        "activate configured ecosystems: ecosystem core is not installed"
    );
}

#[test]
fn test_config_snippet_renders_plugin_configuration() {
    let plugins = plugins();
    assert_eq!(
        config_snippet_with_plugins(
            &Config::with_plugins(&plugins),
            &plugins,
            "main",
            "https://packages.example/cache",
            "client.conf",
        )
        .unwrap(),
        "endpoint = https://packages.example/cache/main/\n"
    );
}

#[test]
fn test_config_snippet_rejects_routes_without_uploads() {
    let plugins = plugins();
    let mut config = Config::with_plugins(&plugins);
    config.indexes[0].route = "read-only".to_owned();

    let error = config_snippet_with_plugins(
        &config,
        &plugins,
        "read-only",
        "https://packages.example",
        "client.conf",
    )
    .unwrap_err();

    assert_eq!(error.to_string(), "index route \"read-only\" does not accept uploads");
}

#[test]
fn test_config_snippet_rejects_invalid_topology() {
    let plugins = plugins();
    let mut config = Config::with_plugins(&plugins);
    config.indexes.push(config.indexes[0].clone());

    let error =
        config_snippet_with_plugins(&config, &plugins, "main", "https://packages.example", "client.conf").unwrap_err();

    assert!(error.to_string().contains("duplicate"), "{error:#}");
}

#[test]
fn test_public_config_snippet_rejects_an_invalid_base_url() {
    let error = config_snippet(&Config::default(), "missing", "not a url", "missing").unwrap_err();

    assert!(error.to_string().contains("base URL"), "{error:#}");
}

#[test]
fn test_public_index_entrypoint_lists_compiled_indexes() {
    let mut output = Vec::new();

    index(&Config::default(), &list_command(None), &mut output).unwrap();

    assert!(
        String::from_utf8(output)
            .unwrap()
            .starts_with("name\troute\tecosystem\tkind\tuploads\n")
    );
}

#[rstest]
#[case::invalid_url("main", "not a url", "client.conf", "base URL")]
#[case::unknown_route("missing", "https://packages.example", "client.conf", "unknown index route")]
#[case::unsupported_format("main", "https://packages.example", "missing", "unsupported snippet format")]
fn test_config_snippet_rejects_invalid_requests(
    #[case] route: &str,
    #[case] base_url: &str,
    #[case] format: &str,
    #[case] expected: &str,
) {
    let plugins = plugins();
    let error =
        config_snippet_with_plugins(&Config::with_plugins(&plugins), &plugins, route, base_url, format).unwrap_err();

    assert!(error.to_string().contains(expected), "{error:#}");
}

fn list_command(ecosystem: Option<String>) -> IndexCommand {
    IndexCommand::List(IndexListArgs {
        runtime: runtime_args(),
        ecosystem,
    })
}

fn show_command(index: &str) -> IndexCommand {
    IndexCommand::Show(IndexShowArgs {
        runtime: runtime_args(),
        index: index.to_owned(),
    })
}

fn detailed_config(plugins: &PluginRegistry) -> Config {
    let mut config = Config::with_plugins(plugins);
    let hosted = config.indexes.remove(0);
    let cached = IndexConfig {
        name: "cache".to_owned(),
        route: "cache".to_owned(),
        kind: IndexKind::Cached {
            routing: UpstreamRoutingConfig {
                upstreams: vec![UpstreamConfig {
                    name: "primary".to_owned(),
                    url: "https://upstream.example/packages/".to_owned(),
                    artifact_url: None,
                    username: None,
                    password: None,
                    token: None,
                    credential_exec: None,
                    credential_refresh: None,
                    tls: UpstreamTlsConfig::default(),
                }],
                fallback: true,
                protected: Vec::new(),
                pins: BTreeMap::new(),
            },
            upstream_concurrency: 1,
            offline: false,
            prefetch: Box::default(),
        },
        ..hosted.clone()
    };
    let combined = IndexConfig {
        name: "combined".to_owned(),
        route: "combined".to_owned(),
        kind: IndexKind::Virtual {
            layers: vec!["cache".to_owned(), "main".to_owned()],
            write_target: Some("main".to_owned()),
        },
        ..hosted.clone()
    };
    config.indexes = vec![hosted, cached, combined];
    config
}
