use std::collections::{BTreeMap, HashSet};

use crate::ast::{Aggregate, AggregateFunc, Ast, CompareOp, Literal, OrderKey, Predicate, Selection};
use crate::catalog::{Column, DomainSchema, FieldClass};
use crate::error::PqlError;
use crate::eval::literal_value;
use crate::source::FetchFilter;
use crate::value::{Value, ValueType};

pub const DEFAULT_LIMIT: u32 = 25;
pub const MAX_LIMIT: u32 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputColumn {
    pub name: String,
    pub class: FieldClass,
    pub value_type: ValueType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub order_by: Vec<OrderKey>,
    pub limit: u32,
    pub outputs: Vec<OutputColumn>,
}

/// # Errors
/// Returns [`PqlError::Validation`] for an unknown or mistyped field or a misused aggregate, and
/// [`PqlError::CostExceeded`] when an unbounded domain is queried without a cheap leading filter.
pub fn plan(ast: &Ast, schema: &DomainSchema) -> Result<Plan, PqlError> {
    let ast = resolve_params(ast, schema)?;
    let plan = validate_resolved(&ast, schema)?;
    cost_gate(&ast, schema)?;
    Ok(plan)
}

pub(crate) fn plan_resolved(ast: &Ast, schema: &DomainSchema) -> Result<Plan, PqlError> {
    let plan = validate_resolved(ast, schema)?;
    cost_gate(ast, schema)?;
    Ok(plan)
}

/// # Errors
/// Returns [`PqlError::UnboundedJoin`] when the probe domain is unbounded, and
/// [`PqlError::CostExceeded`] when the outer domain is unbounded without a cheap leading filter.
pub fn gate_join(ast: &Ast, outer: &DomainSchema, probe: &DomainSchema) -> Result<(), PqlError> {
    if !probe.bounded {
        return Err(PqlError::UnboundedJoin(format!(
            "`{}` is unbounded, so materializing it to build the join is refused",
            probe.name
        )));
    }
    cost_gate(ast, outer)
}

/// # Errors
/// Returns [`PqlError::Validation`] for an unknown or mistyped field or a misused aggregate.
pub fn validate(ast: &Ast, schema: &DomainSchema) -> Result<Plan, PqlError> {
    validate_resolved(&resolve_params(ast, schema)?, schema)
}

pub(crate) fn validate_resolved(ast: &Ast, schema: &DomainSchema) -> Result<Plan, PqlError> {
    if let Some(predicate) = &ast.predicate {
        validate_predicate(predicate, schema)?;
    }
    let limit = resolve_limit(ast.limit)?;
    let outputs = if let Some(aggregate) = &ast.aggregate {
        validate_aggregate(aggregate, schema)?
    } else {
        project(&ast.selection, schema)?
    };
    validate_output_names(&outputs)?;
    let order_by = resolve_order(&ast.order_by, &outputs)?;
    Ok(Plan {
        order_by,
        limit,
        outputs,
    })
}

pub(crate) fn resolve_params(ast: &Ast, schema: &DomainSchema) -> Result<Ast, PqlError> {
    let mut resolved = ast.clone();
    let mut expected = BTreeMap::new();
    if let Some(predicate) = resolved.predicate.take() {
        resolved.predicate = Some(resolve_predicate(predicate, schema, &mut expected)?);
    }
    Ok(resolved)
}

fn resolve_predicate(
    predicate: Predicate,
    schema: &DomainSchema,
    expected: &mut BTreeMap<String, ValueType>,
) -> Result<Predicate, PqlError> {
    match predicate {
        Predicate::Or(left, right) => Ok(Predicate::Or(
            Box::new(resolve_predicate(*left, schema, expected)?),
            Box::new(resolve_predicate(*right, schema, expected)?),
        )),
        Predicate::And(left, right) => Ok(Predicate::And(
            Box::new(resolve_predicate(*left, schema, expected)?),
            Box::new(resolve_predicate(*right, schema, expected)?),
        )),
        Predicate::Not(inner) => Ok(Predicate::Not(Box::new(resolve_predicate(*inner, schema, expected)?))),
        Predicate::Compare { field, op, value } => {
            let column = require_column(&field, schema)?;
            Ok(Predicate::Compare {
                field,
                op,
                value: resolve_literal(value, column, expected)?,
            })
        }
        Predicate::In { field, values } => {
            let column = require_column(&field, schema)?;
            Ok(Predicate::In {
                field,
                values: values
                    .into_iter()
                    .map(|value| resolve_literal(value, column, expected))
                    .collect::<Result<_, _>>()?,
            })
        }
        Predicate::StartsWith { field, prefix } => {
            let column = require_column(&field, schema)?;
            Ok(Predicate::StartsWith {
                field,
                prefix: resolve_literal(prefix, column, expected)?,
            })
        }
    }
}

fn resolve_literal(
    literal: Literal,
    column: &Column,
    expected: &mut BTreeMap<String, ValueType>,
) -> Result<Literal, PqlError> {
    let Literal::BoundParam { name, value } = literal else {
        return Ok(literal);
    };
    if let Some(prior) = expected.insert(name.clone(), column.value_type)
        && prior != column.value_type
    {
        return Err(PqlError::Validation(format!(
            "parameter `:{name}` has incompatible {} and {} column contexts",
            prior.as_str(),
            column.value_type.as_str()
        )));
    }
    match (value, column.value_type) {
        (Value::Bool(value), ValueType::Bool) => Ok(Literal::Bool(value)),
        (Value::Int(value), ValueType::Int) => Ok(Literal::Int(value)),
        (Value::Str(value), ValueType::Str) => Ok(Literal::Str(value)),
        (Value::Timestamp(value), ValueType::Timestamp) => Ok(Literal::Timestamp(value)),
        (Value::Str(value), ValueType::Timestamp) => crate::parse::timestamp_seconds(&value)
            .map(Literal::Timestamp)
            .ok_or_else(|| PqlError::Validation(format!("parameter `:{name}` is not an RFC 3339 timestamp"))),
        _ => Err(PqlError::Validation(format!(
            "the literal does not match the {} column `{}`",
            column.value_type.as_str(),
            column.name
        ))),
    }
}

fn validate_output_names(outputs: &[OutputColumn]) -> Result<(), PqlError> {
    let mut names = HashSet::with_capacity(outputs.len());
    for output in outputs {
        if !names.insert(&output.name) {
            return Err(PqlError::Validation(format!("duplicate output name `{}`", output.name)));
        }
    }
    Ok(())
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
        Literal::Param(_) | Literal::BoundParam { .. } => {
            return Err(PqlError::Validation("a parameter was left unbound".to_owned()));
        }
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

pub(crate) fn require_column<'schema>(field: &str, schema: &'schema DomainSchema) -> Result<&'schema Column, PqlError> {
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
    let column = schema
        .column(field)
        .filter(|column| schema.pushdown.contains(&column.name))?;
    let values: Vec<Value> = literals.iter().map(literal_value).collect();
    if values.iter().any(|value| matches!(value, Value::Null)) {
        return None;
    }
    Some(FetchFilter {
        column: column.name,
        values,
    })
}
