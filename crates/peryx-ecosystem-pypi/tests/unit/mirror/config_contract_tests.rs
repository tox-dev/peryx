use super::{mode, table_bool, table_strings, table_u64};

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
