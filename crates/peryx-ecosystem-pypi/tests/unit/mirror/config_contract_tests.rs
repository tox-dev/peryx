use super::{mode, table_u64};

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
