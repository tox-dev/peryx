use std::path::PathBuf;

use rstest::rstest;

use super::toml_config;
use crate::config::{self, IndexKind, PrefetchMode};

#[test]
fn test_mirror_prefetch_from_toml() {
    let c = toml_config(
        "\
offline = true
read_only = true
[[index]]
name = \"pypi\"
cached = \"https://pypi.org/simple/\"
offline = true

[index.prefetch]
mode = \"metadata-only\"
packages = [\"requests>=2,<3\"]
requirements = [\"requirements.txt\"]
include_wheels = false
include_sdists = true
python_tags = [\"py3\"]
abi_tags = [\"none\"]
platform_tags = [\"any\"]
max_file_size_bytes = 1048576
",
    );
    assert!(c.offline);
    assert!(c.read_only);
    let IndexKind::Cached { offline, prefetch, .. } = &c.indexes[0].kind else {
        panic!("expected cached index");
    };
    assert!(*offline);
    assert_eq!(prefetch.mode, PrefetchMode::MetadataOnly);
    assert_eq!(prefetch.packages, vec!["requests>=2,<3".to_owned()]);
    assert_eq!(prefetch.requirements, vec![PathBuf::from("requirements.txt")]);
    assert!(!prefetch.include_wheels);
    assert!(prefetch.include_sdists);
    assert_eq!(prefetch.python_tags, vec!["py3".to_owned()]);
    assert_eq!(prefetch.abi_tags, vec!["none".to_owned()]);
    assert_eq!(prefetch.platform_tags, vec!["any".to_owned()]);
    assert_eq!(prefetch.max_file_size_bytes, Some(1_048_576));
    assert!(prefetch.metadata_only);
}

#[rstest]
#[case::auto("[index.settings]\nlibrary_prefix = \"auto\"\n", Some(toml::Value::from("auto")))]
#[case::always("[index.settings]\nlibrary_prefix = true\n", Some(toml::Value::from(true)))]
#[case::never("[index.settings]\nlibrary_prefix = false\n", Some(toml::Value::from(false)))]
#[case::absent("", None)]
fn test_index_settings_pass_through_to_the_ecosystem(#[case] settings: &str, #[case] expected: Option<toml::Value>) {
    // The neutral config claims no key of `[index.settings]`: the table reaches the index's ecosystem
    // as the operator wrote it, and only there is a value known to be valid.
    let text = format!(
        "[[index]]\nname = \"hub\"\necosystem = \"oci\"\ncached = \"https://registry-1.docker.io/\"\n{settings}"
    );
    let settings = &toml_config(&text).indexes[0].ecosystem_settings;
    assert_eq!(settings.get("library_prefix").cloned(), expected);
}

#[test]
fn test_index_policy_from_toml() {
    let text = "\
[[index]]\nname = \"pypi\"\ncached = \"https://pypi.org/simple/\"\n\
[index.policy]\nallow_projects = [\"Flask\"]\nblock_projects = [\"bad-pkg\"]\nallow_versions = \">=1,<2\"\n\
allow_package_types = [\"wheel\"]\nblock_package_types = [\"sdist\"]\n\
allow_wheel_pythons = [\"py3\"]\nblock_wheel_pythons = [\"py2\"]\n\
allow_wheel_platforms = [\"any\"]\nblock_wheel_platforms = [\"win_amd64\"]\n\
max_file_size_bytes = 1048576\nmax_project_size_bytes = 10485760\n";
    let config = toml_config(text);
    // The one flat policy table splits: neutral keys land on `policy`, every other key is carried
    // through on `ecosystem_policy` for the index's driver to compile.
    let policy = &config.indexes[0].policy;
    assert_eq!(policy.allow_projects, ["Flask"]);
    assert_eq!(policy.block_projects, ["bad-pkg"]);
    assert_eq!(policy.max_file_size_bytes, Some(1_048_576));
    assert_eq!(policy.max_project_size_bytes, Some(10_485_760));
    let ecosystem = &config.indexes[0].ecosystem_policy;
    assert_eq!(ecosystem["allow_versions"].as_str(), Some(">=1,<2"));
    assert_eq!(ecosystem["allow_package_types"].as_array(), Some(&vec!["wheel".into()]));
    assert_eq!(ecosystem["block_package_types"].as_array(), Some(&vec!["sdist".into()]));
    assert_eq!(ecosystem["allow_wheel_pythons"].as_array(), Some(&vec!["py3".into()]));
    assert_eq!(ecosystem["block_wheel_pythons"].as_array(), Some(&vec!["py2".into()]));
    assert_eq!(ecosystem["allow_wheel_platforms"].as_array(), Some(&vec!["any".into()]));
    assert_eq!(
        ecosystem["block_wheel_platforms"].as_array(),
        Some(&vec!["win_amd64".into()])
    );
    // The neutral keys the engine claimed did not leak into the ecosystem remainder.
    assert!(!ecosystem.contains_key("allow_projects"));
    assert!(!ecosystem.contains_key("max_file_size_bytes"));
}

#[rstest]
#[case::unknown_key("bad.toml", "bogus = 1", Some("bad.toml"))]
#[case::unknown_index_key("x.toml", "[[index]]\nname = \"a\"\nbogus = 1\n", None)]
#[case::non_table_policy(
    "x.toml",
    "[[index]]\nname = \"pypi\"\ncached = \"https://pypi.org/simple/\"\npolicy = 5\n",
    Some("table")
)]
#[case::unknown_log_key("x.toml", "[log]\nbogus = 1\n", None)]
#[case::unknown_rate_limit_key("x.toml", "[rate_limit]\nbogus = 1\n", None)]
#[case::unknown_replication_key(
    "x.toml",
    "[replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"b\"\nbogus = 1\n",
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
