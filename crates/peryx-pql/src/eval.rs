use std::cmp::Ordering;

use crate::ast::{CompareOp, Literal, Predicate};
use crate::value::{Row, Value};

/// Unbound parameters match nothing.
#[must_use]
pub fn evaluate(predicate: &Predicate, row: &Row) -> bool {
    evaluate_nullable(predicate, row).unwrap_or(false)
}

fn evaluate_nullable(predicate: &Predicate, row: &Row) -> Option<bool> {
    match predicate {
        Predicate::Or(left, right) => match (evaluate_nullable(left, row), evaluate_nullable(right, row)) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
        Predicate::And(left, right) => match (evaluate_nullable(left, row), evaluate_nullable(right, row)) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        Predicate::Not(inner) => evaluate_nullable(inner, row).map(|matches| !matches),
        Predicate::Compare { field, op, value } => compare(&row.get(field), *op, &literal_value(value)),
        Predicate::In { field, values } => {
            let cell = row.get(field);
            Some(values.iter().any(|value| cell == literal_value(value)))
        }
        Predicate::StartsWith { field, prefix } => Some(match (row.get(field), literal_value(prefix)) {
            (Value::Str(cell), Value::Str(prefix)) => cell.starts_with(&prefix),
            _ => false,
        }),
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
        Literal::BoundParam { value, .. } => value.clone(),
    }
}

fn compare(left: &Value, op: CompareOp, right: &Value) -> Option<bool> {
    let order = left.compare(right)?;
    Some(match op {
        CompareOp::Eq => order == Ordering::Equal,
        CompareOp::Ne => order != Ordering::Equal,
        CompareOp::Lt => order == Ordering::Less,
        CompareOp::Le => order != Ordering::Greater,
        CompareOp::Gt => order == Ordering::Greater,
        CompareOp::Ge => order != Ordering::Less,
    })
}
