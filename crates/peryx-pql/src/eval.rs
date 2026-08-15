use std::cmp::Ordering;

use crate::ast::{CompareOp, Literal, Predicate};
use crate::value::{Row, Value};

/// Unbound parameters match nothing.
#[must_use]
pub fn evaluate(predicate: &Predicate, row: &Row) -> bool {
    match predicate {
        Predicate::Or(left, right) => evaluate(left, row) || evaluate(right, row),
        Predicate::And(left, right) => evaluate(left, row) && evaluate(right, row),
        Predicate::Not(inner) => !evaluate(inner, row),
        Predicate::Compare { field, op, value } => compare(&row.get(field), *op, &literal_value(value)),
        Predicate::In { field, values } => {
            let cell = row.get(field);
            values.iter().any(|value| cell == literal_value(value))
        }
        Predicate::StartsWith { field, prefix } => match (row.get(field), literal_value(prefix)) {
            (Value::Str(cell), Value::Str(prefix)) => cell.starts_with(&prefix),
            _ => false,
        },
    }
}

#[must_use]
pub fn literal_value(literal: &Literal) -> Value {
    match literal {
        Literal::Str(value) => Value::Str(value.clone()),
        Literal::Int(value) => Value::Int(*value),
        Literal::Bool(value) => Value::Bool(*value),
        Literal::Timestamp(value) => Value::Timestamp(*value),
        Literal::Param(_) => Value::Null,
    }
}

fn compare(left: &Value, op: CompareOp, right: &Value) -> bool {
    match op {
        CompareOp::Eq => left == right,
        CompareOp::Ne => left != right,
        CompareOp::Lt => ordered(left, right, Ordering::Less, false),
        CompareOp::Le => ordered(left, right, Ordering::Less, true),
        CompareOp::Gt => ordered(left, right, Ordering::Greater, false),
        CompareOp::Ge => ordered(left, right, Ordering::Greater, true),
    }
}

fn ordered(left: &Value, right: &Value, wanted: Ordering, or_equal: bool) -> bool {
    match left.compare(right) {
        Some(Ordering::Equal) => or_equal,
        Some(order) => order == wanted,
        None => false,
    }
}
