use std::cmp::Ordering;

use rstest::rstest;
use serde_json::{Value as JsonValue, json};

use crate::value::{Row, Value, ValueType};

#[rstest]
#[case::boolean(ValueType::Bool, "bool")]
#[case::integer(ValueType::Int, "int")]
#[case::string(ValueType::Str, "string")]
#[case::timestamp(ValueType::Timestamp, "timestamp")]
fn test_value_type_name(#[case] value_type: ValueType, #[case] expected: &str) {
    assert_eq!(value_type.as_str(), expected);
}

#[rstest]
#[case::null(Value::Null, None)]
#[case::boolean(Value::Bool(true), Some(ValueType::Bool))]
#[case::integer(Value::Int(1), Some(ValueType::Int))]
#[case::string(Value::Str("a".to_owned()), Some(ValueType::Str))]
#[case::timestamp(Value::Timestamp(1), Some(ValueType::Timestamp))]
fn test_value_type(#[case] value: Value, #[case] expected: Option<ValueType>) {
    assert_eq!(value.value_type(), expected);
}

#[rstest]
#[case::integer(Value::Int(1), Value::Int(2), Some(Ordering::Less))]
#[case::boolean(Value::Bool(true), Value::Bool(false), Some(Ordering::Greater))]
#[case::string(
    Value::Str("a".to_owned()),
    Value::Str("b".to_owned()),
    Some(Ordering::Less)
)]
#[case::timestamp(Value::Timestamp(5), Value::Timestamp(5), Some(Ordering::Equal))]
#[case::mixed(Value::Int(1), Value::Str("a".to_owned()), None)]
#[case::null(Value::Null, Value::Int(1), None)]
#[case::numeric_kinds(Value::Int(1), Value::Timestamp(1), None)]
fn test_value_compare(#[case] left: Value, #[case] right: Value, #[case] expected: Option<Ordering>) {
    assert_eq!(left.compare(&right), expected);
}

#[rstest]
#[case::null(Value::Null, json!(null))]
#[case::boolean(Value::Bool(true), json!(true))]
#[case::integer(Value::Int(7), json!(7))]
#[case::timestamp(Value::Timestamp(9), json!(9))]
#[case::string(Value::Str("x".to_owned()), json!("x"))]
fn test_value_to_json(#[case] value: Value, #[case] expected: JsonValue) {
    assert_eq!(value.to_json(), expected);
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
