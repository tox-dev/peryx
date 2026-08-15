use std::path::PathBuf;

use rstest::rstest;

use super::toml_config;
use crate::config;

#[test]
fn test_writer_identity_from_toml() {
    assert_eq!(
        toml_config("writer_identity = \"writer-a\"\n")
            .writer_identity
            .as_deref(),
        Some("writer-a")
    );
}

#[test]
fn test_node_identity_from_toml() {
    assert_eq!(
        toml_config("node_identity = \"node-b\"\n").node_identity.as_deref(),
        Some("node-b")
    );
}

#[test]
fn test_prefetch_options_remain_opaque() {
    let config = config::from_toml(
        PathBuf::from("config.toml"),
        "\
offline = true
read_only = true
[[index]]
name = \"mirror\"
ecosystem = \"example\"
offline = true
[[index.upstream]]
name = \"primary\"
url = \"https://upstream.example/api/\"

[index.prefetch]
strategy = \"selected\"
selectors = [\"alpha\", \"beta\"]
include_metadata = true
limit = 1048576
",
    )
    .unwrap();
    let index = &config.indexes.as_ref().unwrap()[0];
    assert_eq!(index.ecosystem.as_deref(), Some("example"));
    assert_eq!(index.offline, Some(true));
    assert_eq!(
        index.prefetch.as_ref().unwrap().options,
        toml::from_str(
            r#"
strategy = "selected"
selectors = ["alpha", "beta"]
include_metadata = true
limit = 1048576
"#,
        )
        .unwrap()
    );
}

#[rstest]
#[case::string("[index.settings]\nmode = \"strict\"\n", Some(toml::Value::from("strict")))]
#[case::boolean("[index.settings]\nmode = true\n", Some(toml::Value::from(true)))]
#[case::integer("[index.settings]\nmode = 2\n", Some(toml::Value::from(2)))]
#[case::absent("", None)]
fn test_index_settings_pass_through_to_the_ecosystem(#[case] settings: &str, #[case] expected: Option<toml::Value>) {
    let text = format!(
        "[[index]]\nname = \"mirror\"\necosystem = \"example\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://upstream.example/api/\"\n{settings}"
    );
    let config = config::from_toml(PathBuf::from("config.toml"), &text).unwrap();
    assert_eq!(
        config.indexes.as_ref().unwrap()[0].settings.get("mode").cloned(),
        expected
    );
}

#[test]
fn test_index_policy_splits_neutral_and_plugin_keys() {
    let text = "\
[[index]]\nname = \"mirror\"\necosystem = \"example\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://upstream.example/api/\"\n\
[index.policy]\nallow_resources = [\"alpha\"]\nblock_resources = [\"blocked\"]\nversion_rule = \">=1,<2\"\n\
artifact_types = [\"binary\", \"source\"]\n\
max_artifact_size_bytes = 1048576\nmax_resource_size_bytes = 10485760\n";
    let config = config::from_toml(PathBuf::from("config.toml"), text).unwrap();
    let index = &config.indexes.as_ref().unwrap()[0];
    let policy = &index.policy.neutral;
    assert_eq!(policy.allow_resources, ["alpha"]);
    assert_eq!(policy.block_resources, ["blocked"]);
    assert_eq!(policy.max_artifact_size_bytes, Some(1_048_576));
    assert_eq!(policy.max_resource_size_bytes, Some(10_485_760));
    let ecosystem = &index.policy.ecosystem;
    assert_eq!(
        ecosystem,
        &toml::from_str(
            r#"
version_rule = ">=1,<2"
artifact_types = ["binary", "source"]
"#,
        )
        .unwrap()
    );
}

#[rstest]
#[case::unknown_key("bad.toml", "bogus = 1", Some("bad.toml"))]
#[case::unknown_index_key("x.toml", "[[index]]\nname = \"a\"\nbogus = 1\n", None)]
#[case::non_table_policy("x.toml", "[[index]]\nname = \"index\"\npolicy = 5\n", Some("table"))]
#[case::unknown_log_key("x.toml", "[log]\nbogus = 1\n", None)]
#[case::unknown_rate_limit_key("x.toml", "[rate_limit]\nbogus = 1\n", None)]
#[case::unknown_availability_key("x.toml", "[availability]\nmode = \"none\"\nbogus = 1\n", None)]
#[case::unknown_replication_key(
    "x.toml",
    "[availability]\nmode = \"dc\"\n[availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"b\"\nbogus = 1\n",
    None
)]
#[case::invalid_trusted_proxy("x.toml", "[rate_limit]\ntrusted_proxies = [\"invalid\"]\n", Some("trusted_proxies"))]
#[case::invalid_log_format("x.toml", "[log]\nformat = \"xml\"\n", None)]
#[case::invalid_log_sink("x.toml", "[log]\nsink = \"kafka\"\n", None)]
fn test_from_toml_rejects(#[case] path: &str, #[case] text: &str, #[case] expected: Option<&str>) {
    let err = config::from_toml(PathBuf::from(path), text).unwrap_err();
    if let Some(substr) = expected {
        assert!(err.to_string().contains(substr), "{err}");
    }
}
