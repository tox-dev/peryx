use peryx_driver::serving::MirrorDriver as _;

use super::{PrefetchCounts, mode, table_bool, table_strings, table_u64};
use crate::PypiServing;

#[rstest::rstest]
#[case::configured("images", false)]
#[case::override_option("images", true)]
fn prefetch_options_reject_unsupported_keys(#[case] key: &str, #[case] override_option: bool) {
    let mut configured = toml::Table::new();
    let mut overrides = toml::Table::new();
    if override_option {
        overrides.insert(key.to_owned(), toml::Value::Array(Vec::new()));
    } else {
        configured.insert(key.to_owned(), toml::Value::Array(Vec::new()));
    }

    assert_eq!(
        PypiServing.validate_options(&configured, &overrides).unwrap_err(),
        "prefetch option \"images\" is not supported by pypi"
    );
}

#[test]
fn prefetch_options_accept_consumed_keys() {
    let configured = toml::Table::from_iter(
        [
            "mode",
            "packages",
            "requirements",
            "include_wheels",
            "include_sdists",
            "python_tags",
            "abi_tags",
            "platform_tags",
            "max_file_size_bytes",
            "metadata_only",
        ]
        .map(|key| (key.to_owned(), toml::Value::Boolean(true))),
    );
    let overrides = toml::Table::from_iter(
        [
            "packages",
            "requirements",
            "mode",
            "metadata_only",
            "no_wheels",
            "no_sdists",
            "python_tags",
            "abi_tags",
            "platform_tags",
            "max_file_size_bytes",
        ]
        .map(|key| (key.to_owned(), toml::Value::Boolean(true))),
    );

    assert_eq!(PypiServing.validate_options(&configured, &overrides), Ok(()));
}

#[test]
fn configuration_rejects_unknown_modes_and_invalid_sizes() {
    assert_eq!(mode("unknown").unwrap_err(), "unknown mirror mode \"unknown\"");
    for value in [
        toml::Value::Integer(-1),
        toml::Value::String("large".to_owned()),
        toml::Value::Boolean(true),
    ] {
        let table = toml::Table::from_iter([("size".to_owned(), value)]);
        assert_eq!(table_u64(&table, "size").unwrap_err(), "size must be an integer");
    }
}

#[test]
fn configuration_reads_string_lists_and_booleans() {
    let table = toml::Table::from_iter([
        (
            "packages".to_owned(),
            toml::Value::Array(vec![toml::Value::String("demo".to_owned())]),
        ),
        ("wheels".to_owned(), toml::Value::Boolean(false)),
    ]);
    assert_eq!(table_strings(&table, "packages").unwrap(), ["demo"]);
    assert!(!table_bool(&table, "wheels", true).unwrap());
    assert!(table_strings(&table, "missing").unwrap().is_empty());
    assert!(table_bool(&table, "missing", true).unwrap());
}

#[test]
fn configuration_reads_a_size_given_as_a_string() {
    let table = toml::Table::from_iter([("size".to_owned(), toml::Value::String("42".to_owned()))]);

    assert_eq!(table_u64(&table, "size").unwrap(), Some(42));
}

#[test]
fn prefetch_counts_merge_sums_the_files_field() {
    let mut counts = PrefetchCounts {
        files: 2,
        ..PrefetchCounts::default()
    };
    let other = PrefetchCounts {
        files: 3,
        ..PrefetchCounts::default()
    };

    counts.merge(&other);

    assert_eq!(counts.files, 5);
}

#[test]
fn configuration_rejects_invalid_string_lists_and_booleans() {
    for value in [
        toml::Value::String("demo".to_owned()),
        toml::Value::Array(vec![toml::Value::Integer(1)]),
    ] {
        let table = toml::Table::from_iter([("packages".to_owned(), value)]);
        assert!(table_strings(&table, "packages").is_err());
    }
    let table = toml::Table::from_iter([("wheels".to_owned(), toml::Value::String("yes".to_owned()))]);
    assert!(table_bool(&table, "wheels", true).is_err());
}
