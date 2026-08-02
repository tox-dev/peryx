//! The executor: scope injection, filtering, aggregation, ordering, and paging.
//!
//! The executor is where authorization becomes structural. It injects the caller's scope as a
//! predicate `ANDed` below any user predicate and applies it before ordering and paging, so counts and
//! pagination run over authorized rows only — there is no post-limit filtering that could leak a
//! total. What it never does is write: it reads rows from a [`DataSource`] and reduces them.

use std::cmp::Ordering;

use crate::ast::{AggregateFunc, Ast, OrderKey};
use crate::cursor;
use crate::error::PqlError;
use crate::eval::evaluate;
use crate::plan::{OutputColumn, Plan, plan};
use crate::scope::{QueryScope, RepoScope};
use crate::source::DataSource;
use crate::value::{Row, Value};

/// The column every repository-scoped domain exposes and the executor injects scope on.
const SCOPE_COLUMN: &str = "repository";

/// One page of results: the output columns, the rows as value tuples aligned to those columns, and
/// an opaque cursor for the next page when one exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub outputs: Vec<OutputColumn>,
    pub rows: Vec<Vec<Value>>,
    pub next_cursor: Option<String>,
}

/// Plan and run a query over a data source, returning one page.
///
/// # Errors
/// Propagates planning, cost, cursor, and backend errors. Returns [`PqlError::Unauthorized`] when the
/// source does not serve the named domain, so existence is not disclosed.
pub fn execute(
    ast: &Ast,
    scope: &QueryScope,
    cursor_text: Option<&str>,
    source: &dyn DataSource,
) -> Result<Page, PqlError> {
    let schema = source.schema(&ast.domain).ok_or(PqlError::Unauthorized)?;
    let plan = plan(ast, schema)?;
    let has_scope_column = schema.column(SCOPE_COLUMN).is_some();
    let offset = match cursor_text {
        Some(text) => cursor::decode(text, &ast.domain, scope)?,
        None => 0,
    };
    let rows = source.fetch(&ast.domain, scope)?;
    let filtered = filter_rows(rows, ast, scope, has_scope_column);
    let mut tuples = if let Some(aggregate) = &ast.aggregate {
        aggregate_rows(&filtered, aggregate, &plan.outputs)
    } else {
        project_rows(&filtered, &plan.outputs)
    };
    order_rows(&mut tuples, &resolved_order(&plan, schema.natural_order), &plan.outputs);
    Ok(paginate(tuples, plan, &ast.domain, scope, offset))
}

fn filter_rows(rows: Vec<Row>, ast: &Ast, scope: &QueryScope, has_scope_column: bool) -> Vec<Row> {
    rows.into_iter()
        .filter(|row| in_scope(row, scope, has_scope_column))
        .filter(|row| ast.predicate.as_ref().is_none_or(|predicate| evaluate(predicate, row)))
        .collect()
}

fn in_scope(row: &Row, scope: &QueryScope, has_scope_column: bool) -> bool {
    if !has_scope_column {
        return true;
    }
    match scope.repositories() {
        RepoScope::All => true,
        RepoScope::Only(_) => match row.get(SCOPE_COLUMN) {
            Value::Str(repository) => scope.repositories().permits(&repository),
            _ => false,
        },
    }
}

fn project_rows(rows: &[Row], outputs: &[OutputColumn]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| outputs.iter().map(|output| row.get(&output.name)).collect())
        .collect()
}

fn aggregate_rows(rows: &[Row], aggregate: &crate::ast::Aggregate, outputs: &[OutputColumn]) -> Vec<Vec<Value>> {
    let mut groups: Vec<(Vec<Value>, Vec<Accumulator>)> = Vec::new();
    for row in rows {
        let key: Vec<Value> = aggregate.group_by.iter().map(|name| row.get(name)).collect();
        let slot = groups.iter_mut().find(|(existing, _)| *existing == key);
        let accumulators = if let Some((_, accumulators)) = slot {
            accumulators
        } else {
            let fresh = aggregate.terms.iter().map(|term| Accumulator::new(term.func)).collect();
            groups.push((key, fresh));
            &mut groups.last_mut().expect("just pushed").1
        };
        for (accumulator, term) in accumulators.iter_mut().zip(&aggregate.terms) {
            accumulator.observe(term.column.as_deref().map(|name| row.get(name)));
        }
    }
    groups
        .into_iter()
        .map(|(key, accumulators)| finish_group(key, accumulators, outputs))
        .collect()
}

fn finish_group(mut key: Vec<Value>, accumulators: Vec<Accumulator>, outputs: &[OutputColumn]) -> Vec<Value> {
    debug_assert_eq!(key.len() + accumulators.len(), outputs.len());
    key.extend(accumulators.into_iter().map(Accumulator::finish));
    key
}

#[derive(Debug)]
enum Accumulator {
    Count(i64),
    Sum(i64),
    Min(Option<Value>),
    Max(Option<Value>),
}

impl Accumulator {
    const fn new(func: AggregateFunc) -> Self {
        match func {
            AggregateFunc::Count => Self::Count(0),
            AggregateFunc::Sum => Self::Sum(0),
            AggregateFunc::Min => Self::Min(None),
            AggregateFunc::Max => Self::Max(None),
        }
    }

    fn observe(&mut self, value: Option<Value>) {
        match self {
            Self::Count(count) => *count += 1,
            Self::Sum(total) => {
                if let Some(number) = value.as_ref().and_then(numeric) {
                    *total += number;
                }
            }
            Self::Min(current) => keep(current, value, Ordering::Less),
            Self::Max(current) => keep(current, value, Ordering::Greater),
        }
    }

    fn finish(self) -> Value {
        match self {
            Self::Count(count) | Self::Sum(count) => Value::Int(count),
            Self::Min(value) | Self::Max(value) => value.unwrap_or(Value::Null),
        }
    }
}

fn keep(current: &mut Option<Value>, value: Option<Value>, wanted: Ordering) {
    let Some(candidate) = value.filter(|value| !matches!(value, Value::Null)) else {
        return;
    };
    let replace = current
        .as_ref()
        .is_none_or(|held| candidate.compare(held) == Some(wanted));
    if replace {
        *current = Some(candidate);
    }
}

const fn numeric(value: &Value) -> Option<i64> {
    match value {
        Value::Int(number) | Value::Timestamp(number) => Some(*number),
        _ => None,
    }
}

fn resolved_order(plan: &Plan, natural_order: &str) -> Vec<OrderKey> {
    if !plan.order_by.is_empty() {
        return plan.order_by.clone();
    }
    if plan.outputs.iter().any(|output| output.name == natural_order) {
        return vec![OrderKey {
            field: natural_order.to_owned(),
            descending: true,
        }];
    }
    Vec::new()
}

fn order_rows(rows: &mut [Vec<Value>], order_by: &[OrderKey], outputs: &[OutputColumn]) {
    if order_by.is_empty() {
        return;
    }
    let keyed: Vec<(usize, bool)> = order_by
        .iter()
        .filter_map(|key| {
            outputs
                .iter()
                .position(|output| output.name == key.field)
                .map(|index| (index, key.descending))
        })
        .collect();
    rows.sort_by(|left, right| compare_tuples(left, right, &keyed));
}

fn compare_tuples(left: &[Value], right: &[Value], keyed: &[(usize, bool)]) -> Ordering {
    for (index, descending) in keyed {
        let order = left[*index].compare(&right[*index]).unwrap_or(Ordering::Equal);
        let order = if *descending { order.reverse() } else { order };
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}

fn paginate(mut rows: Vec<Vec<Value>>, plan: Plan, domain: &str, scope: &QueryScope, offset: u64) -> Page {
    let total = rows.len();
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
    let limit = plan.limit as usize;
    let end = start.saturating_add(limit).min(total);
    let next_cursor = (end < total).then(|| cursor::encode(domain, scope, end as u64));
    rows.truncate(end);
    rows.drain(..start);
    Page {
        outputs: plan.outputs,
        rows,
        next_cursor,
    }
}
