//! Validation, the cost gate, and the resolved plan.
//!
//! Planning turns a bound [`Ast`] and a [`DomainSchema`] into a [`Plan`]: it type-checks every field
//! against the catalog, refuses aggregates that are not declared, defaults ordering and paging, and
//! runs the static cost gate. The gate reads the source's own indexability rather than guessing, so
//! an unbounded domain with no cheap leading filter is refused before it runs, not after.

use crate::ast::{Aggregate, AggregateFunc, Ast, CompareOp, Literal, OrderKey, Predicate, Selection};
use crate::catalog::{Column, DomainSchema, FieldClass};
use crate::error::PqlError;
use crate::eval::literal_value;
use crate::source::FetchFilter;
use crate::value::{Value, ValueType};

/// The default page size for an interactive read.
pub const DEFAULT_LIMIT: u32 = 25;
/// The largest page an interactive read may request.
pub const MAX_LIMIT: u32 = 100;

/// One column of the result, with the authority needed to read it and its type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputColumn {
    pub name: String,
    pub class: FieldClass,
    pub value_type: ValueType,
}

/// A validated, cost-checked plan ready to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub order_by: Vec<OrderKey>,
    pub limit: u32,
    pub outputs: Vec<OutputColumn>,
}

/// Validate and cost a query against one domain's schema.
///
/// # Errors
/// Returns [`PqlError::JoinUnavailable`] when the query declares a join, [`PqlError::Validation`]
/// for an unknown or mistyped field or a misused aggregate, and [`PqlError::CostExceeded`] when an
/// unbounded domain is queried without a cheap leading filter.
pub fn plan(ast: &Ast, schema: &DomainSchema) -> Result<Plan, PqlError> {
    if ast.join.is_some() {
        return Err(PqlError::JoinUnavailable);
    }
    if let Some(predicate) = &ast.predicate {
        validate_predicate(predicate, schema)?;
    }
    let limit = resolve_limit(ast.limit)?;
    let outputs = if let Some(aggregate) = &ast.aggregate {
        validate_aggregate(aggregate, schema)?
    } else {
        project(&ast.selection, schema)?
    };
    let order_by = resolve_order(&ast.order_by, &outputs)?;
    cost_gate(ast, schema)?;
    Ok(Plan {
        order_by,
        limit,
        outputs,
    })
}

fn resolve_limit(limit: Option<u32>) -> Result<u32, PqlError> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(0) => Err(PqlError::Validation("limit must be at least 1".to_owned())),
        Some(value) if value <= MAX_LIMIT => Ok(value),
        Some(_) => Err(PqlError::Validation(format!("limit must be at most {MAX_LIMIT}"))),
    }
}

fn validate_predicate(predicate: &Predicate, schema: &DomainSchema) -> Result<(), PqlError> {
    match predicate {
        Predicate::Or(left, right) | Predicate::And(left, right) => {
            validate_predicate(left, schema)?;
            validate_predicate(right, schema)
        }
        Predicate::Not(inner) => validate_predicate(inner, schema),
        Predicate::Compare { field, op, value } => validate_compare(field, *op, value, schema),
        Predicate::In { field, values } => {
            let column = require_column(field, schema)?;
            for value in values {
                check_literal_type(column, value)?;
            }
            Ok(())
        }
        Predicate::StartsWith { field, prefix } => {
            let column = require_column(field, schema)?;
            if column.value_type != ValueType::Str {
                return Err(PqlError::Validation(format!(
                    "`starts_with` needs a string column, but `{field}` is {}",
                    column.value_type.as_str()
                )));
            }
            check_literal_type(column, prefix)
        }
    }
}

fn validate_compare(field: &str, op: CompareOp, value: &Literal, schema: &DomainSchema) -> Result<(), PqlError> {
    let column = require_column(field, schema)?;
    if matches!(op, CompareOp::Lt | CompareOp::Le | CompareOp::Gt | CompareOp::Ge)
        && matches!(column.value_type, ValueType::Bool | ValueType::Str)
    {
        return Err(PqlError::Validation(format!(
            "`{}` is not defined for the {} column `{field}`",
            op.as_str(),
            column.value_type.as_str()
        )));
    }
    check_literal_type(column, value)
}

fn check_literal_type(column: &Column, literal: &Literal) -> Result<(), PqlError> {
    let matches = match literal {
        Literal::Param(_) => return Err(PqlError::Validation("a parameter was left unbound".to_owned())),
        Literal::Str(_) => column.value_type == ValueType::Str,
        Literal::Bool(_) => column.value_type == ValueType::Bool,
        Literal::Int(_) => column.value_type == ValueType::Int,
        Literal::Timestamp(_) => column.value_type == ValueType::Timestamp,
    };
    if matches {
        Ok(())
    } else {
        Err(PqlError::Validation(format!(
            "the literal does not match the {} column `{}`",
            column.value_type.as_str(),
            column.name
        )))
    }
}

fn require_column<'schema>(field: &str, schema: &'schema DomainSchema) -> Result<&'schema Column, PqlError> {
    schema
        .column(field)
        .ok_or_else(|| PqlError::Validation(format!("unknown column `{field}`")))
}

fn project(selection: &Selection, schema: &DomainSchema) -> Result<Vec<OutputColumn>, PqlError> {
    match selection {
        Selection::All => Ok(schema.columns.iter().map(output_of).collect()),
        Selection::Columns(names) => names
            .iter()
            .map(|name| require_column(name, schema).map(output_of))
            .collect(),
    }
}

fn output_of(column: &Column) -> OutputColumn {
    OutputColumn {
        name: column.name.to_owned(),
        class: column.class,
        value_type: column.value_type,
    }
}

fn validate_aggregate(aggregate: &Aggregate, schema: &DomainSchema) -> Result<Vec<OutputColumn>, PqlError> {
    let mut outputs = Vec::new();
    for key in &aggregate.group_by {
        let column = require_column(key, schema)?;
        if !schema.bounded && !column.indexability.is_cheap() {
            return Err(PqlError::Validation(format!(
                "group key `{key}` is not cheap to group on"
            )));
        }
        outputs.push(output_of(column));
    }
    for term in &aggregate.terms {
        outputs.push(validate_aggregate_term(term, schema)?);
    }
    Ok(outputs)
}

fn validate_aggregate_term(term: &crate::ast::AggregateTerm, schema: &DomainSchema) -> Result<OutputColumn, PqlError> {
    if term.alias.is_empty() {
        return Err(PqlError::Validation("an aggregate needs an alias".to_owned()));
    }
    match (&term.column, term.func.needs_column()) {
        (Some(name), true) => {
            let column = require_column(name, schema)?;
            if !column.numeric {
                return Err(PqlError::Validation(format!(
                    "`{}` needs a numeric column, but `{name}` is not numeric",
                    term.func.as_str()
                )));
            }
            Ok(OutputColumn {
                name: term.alias.clone(),
                class: column.class,
                value_type: aggregate_type(term.func, column.value_type),
            })
        }
        (None, false) => Ok(OutputColumn {
            name: term.alias.clone(),
            class: FieldClass::Public,
            value_type: ValueType::Int,
        }),
        (Some(_), false) => Err(PqlError::Validation("`count` takes no column".to_owned())),
        (None, true) => Err(PqlError::Validation(format!("`{}` needs a column", term.func.as_str()))),
    }
}

const fn aggregate_type(func: AggregateFunc, column: ValueType) -> ValueType {
    match func {
        AggregateFunc::Count | AggregateFunc::Sum => ValueType::Int,
        AggregateFunc::Min | AggregateFunc::Max => column,
    }
}

fn resolve_order(order_by: &[OrderKey], outputs: &[OutputColumn]) -> Result<Vec<OrderKey>, PqlError> {
    for key in order_by {
        if !outputs.iter().any(|output| output.name == key.field) {
            return Err(PqlError::Validation(format!(
                "cannot order by `{}`; it is not a selected column",
                key.field
            )));
        }
    }
    Ok(order_by.to_vec())
}

fn cost_gate(ast: &Ast, schema: &DomainSchema) -> Result<(), PqlError> {
    if schema.bounded {
        return Ok(());
    }
    if ast
        .predicate
        .as_ref()
        .and_then(|predicate| leading_filter(predicate, schema))
        .is_some()
    {
        Ok(())
    } else {
        Err(PqlError::CostExceeded(format!(
            "`{}` is large; add an equality filter on an indexed column",
            schema.name
        )))
    }
}

/// The cost gate's leading filter for a predicate over a schema: the first equality or membership on
/// a cheaply-indexed column, lowered to concrete values.
///
/// This is both what admits an unbounded domain (a `Some` result means the query is affordable) and
/// what the executor hands to [`crate::source::DataSource::fetch`] so the source narrows through its
/// index rather than scanning the whole domain (constraint 6). A leading `and` term satisfies it; an
/// `or`, `not`, or an unbound parameter does not.
#[must_use]
pub fn leading_filter(predicate: &Predicate, schema: &DomainSchema) -> Option<FetchFilter> {
    match predicate {
        Predicate::And(left, right) => leading_filter(left, schema).or_else(|| leading_filter(right, schema)),
        Predicate::Compare {
            field,
            op: CompareOp::Eq,
            value,
        } => cheap_filter(field, std::slice::from_ref(value), schema),
        Predicate::In { field, values } => cheap_filter(field, values, schema),
        _ => None,
    }
}

fn cheap_filter(field: &str, literals: &[Literal], schema: &DomainSchema) -> Option<FetchFilter> {
    let column = schema.column(field).filter(|column| column.indexability.is_cheap())?;
    let values: Vec<Value> = literals.iter().map(literal_value).collect();
    if values.iter().any(|value| matches!(value, Value::Null)) {
        return None;
    }
    Some(FetchFilter {
        column: column.name,
        values,
    })
}
