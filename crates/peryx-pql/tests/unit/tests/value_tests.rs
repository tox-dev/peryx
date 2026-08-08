use std::cmp::Ordering;

use serde_json::json;

use crate::value::{Row, Value, ValueType};

#[test]
fn test_value_type_names_are_lowercase() {
    assert_eq!(ValueType::Bool.as_str(), "bool");
    assert_eq!(ValueType::Int.as_str(), "int");
    assert_eq!(ValueType::Str.as_str(), "string");
    assert_eq!(ValueType::Timestamp.as_str(), "timestamp");
}

#[test]
fn test_value_type_of_each_variant() {
    assert_eq!(Value::Null.value_type(), None);
    assert_eq!(Value::Bool(true).value_type(), Some(ValueType::Bool));
    assert_eq!(Value::Int(1).value_type(), Some(ValueType::Int));
    assert_eq!(Value::Str("a".to_owned()).value_type(), Some(ValueType::Str));
    assert_eq!(Value::Timestamp(1).value_type(), Some(ValueType::Timestamp));
}

#[test]
fn test_compare_orders_same_kinds() {
    assert_eq!(Value::Int(1).compare(&Value::Int(2)), Some(Ordering::Less));
    assert_eq!(Value::Bool(true).compare(&Value::Bool(false)), Some(Ordering::Greater));
    assert_eq!(
        Value::Str("a".to_owned()).compare(&Value::Str("b".to_owned())),
        Some(Ordering::Less)
    );
    assert_eq!(Value::Timestamp(5).compare(&Value::Timestamp(5)), Some(Ordering::Equal));
}

#[test]
fn test_compare_of_mixed_or_null_is_none() {
    assert_eq!(Value::Int(1).compare(&Value::Str("a".to_owned())), None);
    assert_eq!(Value::Null.compare(&Value::Int(1)), None);
    assert_eq!(Value::Int(1).compare(&Value::Timestamp(1)), None);
}

#[test]
fn test_to_json_renders_each_variant() {
    assert_eq!(Value::Null.to_json(), json!(null));
    assert_eq!(Value::Bool(true).to_json(), json!(true));
    assert_eq!(Value::Int(7).to_json(), json!(7));
    assert_eq!(Value::Timestamp(9).to_json(), json!(9));
    assert_eq!(Value::Str("x".to_owned()).to_json(), json!("x"));
}

#[test]
fn test_row_reads_present_and_absent_cells() {
    let row = Row::new().with("a", Value::Int(1));
    assert_eq!(row.get("a"), Value::Int(1));
    assert_eq!(row.get("missing"), Value::Null);
}

#[test]
fn test_row_default_is_empty() {
    assert_eq!(Row::default().get("any"), Value::Null);
}
