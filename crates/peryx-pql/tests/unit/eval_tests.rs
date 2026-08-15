use crate::ast::{CompareOp, Literal, Predicate};
use crate::eval::{evaluate, literal_value};
use crate::value::{Row, Value};

fn compare(field: &str, op: CompareOp, value: Literal) -> Predicate {
    Predicate::Compare {
        field: field.to_owned(),
        op,
        value,
    }
}

fn row() -> Row {
    Row::new()
        .with("n", Value::Int(5))
        .with("s", Value::Str("resource-a".to_owned()))
        .with("flag", Value::Bool(true))
        .with("at", Value::Timestamp(100))
}

#[test]
fn test_literal_value_lowers_each_literal() {
    assert_eq!(literal_value(&Literal::Str("a".to_owned())), Value::Str("a".to_owned()));
    assert_eq!(literal_value(&Literal::Int(1)), Value::Int(1));
    assert_eq!(literal_value(&Literal::Bool(false)), Value::Bool(false));
    assert_eq!(literal_value(&Literal::Timestamp(2)), Value::Timestamp(2));
    assert_eq!(literal_value(&Literal::Param("p".to_owned())), Value::Null);
}

#[test]
fn test_evaluate_comparisons() {
    let row = row();
    assert!(evaluate(&compare("n", CompareOp::Eq, Literal::Int(5)), &row));
    assert!(evaluate(&compare("n", CompareOp::Ne, Literal::Int(6)), &row));
    assert!(evaluate(&compare("n", CompareOp::Lt, Literal::Int(6)), &row));
    assert!(evaluate(&compare("n", CompareOp::Le, Literal::Int(5)), &row));
    assert!(evaluate(&compare("n", CompareOp::Gt, Literal::Int(4)), &row));
    assert!(evaluate(&compare("n", CompareOp::Ge, Literal::Int(5)), &row));
    assert!(!evaluate(&compare("n", CompareOp::Gt, Literal::Int(5)), &row));
}

#[test]
fn test_evaluate_incomparable_never_matches_ordering() {
    let row = row();
    assert!(!evaluate(&compare("s", CompareOp::Lt, Literal::Int(1)), &row));
    assert!(!evaluate(&compare("missing", CompareOp::Eq, Literal::Int(1)), &row));
    assert!(evaluate(&compare("missing", CompareOp::Ne, Literal::Int(1)), &row));
}

#[test]
fn test_evaluate_boolean_logic() {
    let row = row();
    let yes = compare("n", CompareOp::Eq, Literal::Int(5));
    let no = compare("n", CompareOp::Eq, Literal::Int(9));
    assert!(evaluate(
        &Predicate::And(Box::new(yes.clone()), Box::new(yes.clone())),
        &row
    ));
    assert!(!evaluate(
        &Predicate::And(Box::new(yes.clone()), Box::new(no.clone())),
        &row
    ));
    assert!(evaluate(&Predicate::Or(Box::new(no.clone()), Box::new(yes)), &row));
    assert!(evaluate(&Predicate::Not(Box::new(no)), &row));
}

#[test]
fn test_evaluate_membership() {
    let row = row();
    let hit = Predicate::In {
        field: "s".to_owned(),
        values: vec![
            Literal::Str("resource-b".to_owned()),
            Literal::Str("resource-a".to_owned()),
        ],
    };
    let miss = Predicate::In {
        field: "s".to_owned(),
        values: vec![Literal::Str("resource-b".to_owned())],
    };
    assert!(evaluate(&hit, &row));
    assert!(!evaluate(&miss, &row));
}

#[test]
fn test_evaluate_starts_with() {
    let row = row();
    let hit = Predicate::StartsWith {
        field: "s".to_owned(),
        prefix: Literal::Str("resource".to_owned()),
    };
    let miss = Predicate::StartsWith {
        field: "s".to_owned(),
        prefix: Literal::Str("sci".to_owned()),
    };
    let wrong_type = Predicate::StartsWith {
        field: "n".to_owned(),
        prefix: Literal::Str("5".to_owned()),
    };
    assert!(evaluate(&hit, &row));
    assert!(!evaluate(&miss, &row));
    assert!(!evaluate(&wrong_type, &row));
}

#[test]
fn test_evaluate_timestamp_and_bool_equality() {
    let row = row();
    assert!(evaluate(&compare("at", CompareOp::Ge, Literal::Timestamp(100)), &row));
    assert!(evaluate(&compare("flag", CompareOp::Eq, Literal::Bool(true)), &row));
}
