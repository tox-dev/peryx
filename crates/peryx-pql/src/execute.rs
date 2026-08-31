use std::cmp::Ordering;
use std::collections::HashMap;

use crate::ast::{AggregateFunc, Ast, Join, OrderKey, Predicate};
use crate::catalog::DomainSchema;
use crate::cursor;
use crate::error::PqlError;
use crate::eval::evaluate;
use crate::plan::{
    OutputColumn, Plan, gate_join, leading_filter, plan_resolved, require_column, resolve_params, validate_resolved,
};
use crate::scope::{QueryScope, RepoScope};
use crate::source::DataSource;
use crate::value::{Row, Value};

/// Repository-scoped domains must expose this column.
const SCOPE_COLUMN: &str = "repository";

/// The most key matches a join may walk. Fan-out is the product of the two cardinalities, so a key
/// both sides repeat outruns memory long before pagination sees a row. Storage caps a repository's
/// decision history at 10,000, which keeps the widest one-to-one join well inside this bound.
const MAX_JOIN_MATCHES: usize = 25_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub outputs: Vec<OutputColumn>,
    pub rows: Vec<Vec<Value>>,
    pub next_cursor: Option<String>,
}

/// # Errors
/// Propagates planning, cost, cursor, and backend errors. Returns [`PqlError::Unauthorized`] when the
/// source does not serve the named domain, so existence is not disclosed.
pub fn execute(
    ast: &Ast,
    scope: &QueryScope,
    cursor_text: Option<&str>,
    source: &dyn DataSource,
) -> Result<Page, PqlError> {
    if let Some(join) = &ast.join {
        return execute_join(ast, join, scope, cursor_text, source);
    }
    let schema = source.schema(&ast.domain).ok_or(PqlError::Unauthorized)?;
    let visible = visible_schema(schema, scope)?;
    let ast = resolve_params(ast, &visible)?;
    let plan = plan_resolved(&ast, &visible)?;
    let filter = ast
        .predicate
        .as_ref()
        .and_then(|predicate| leading_filter(predicate, &visible));
    let offset = decode_offset(cursor_text, &ast.domain, scope)?;
    let fetched = scope_filter(source.fetch(&ast.domain, scope, filter.as_ref())?, scope, schema);
    Ok(finish(
        retain_matching(fetched, ast.predicate.as_ref()),
        &ast,
        plan,
        visible.natural_order,
        &ast.domain,
        scope,
        offset,
    ))
}

/// Planning, the cost gate, and projection all run against the caller's own schema, so a column
/// above their field class cannot reach a predicate, an order term, a group key, or an aggregate,
/// and a rejected query fetches no rows. Row scoping keeps the full schema: a hidden `repository`
/// column must still confine the caller to their repositories.
///
/// A domain whose natural order is hidden cannot be paged without disclosing that column's order,
/// so it reads as a domain the caller cannot see.
fn visible_schema(schema: &DomainSchema, scope: &QueryScope) -> Result<DomainSchema, PqlError> {
    let visible = schema.visible_to(scope.visibility());
    if visible.column(visible.natural_order).is_none() {
        return Err(PqlError::Unauthorized);
    }
    Ok(visible)
}

fn execute_join(
    ast: &Ast,
    join: &Join,
    scope: &QueryScope,
    cursor_text: Option<&str>,
    source: &dyn DataSource,
) -> Result<Page, PqlError> {
    let schema_a = source.schema(&ast.domain).ok_or(PqlError::Unauthorized)?;
    let schema_b = source.schema(&join.domain).ok_or(PqlError::Unauthorized)?;
    // Merging before hiding is what makes a shared column carry its stricter side's class: hiding
    // each side first would let the caller keep the laxer copy while the row still holds the
    // stricter value.
    let merged = visible_schema(&merge_schemas(schema_a, schema_b), scope)?;
    validate_join(&join.on, schema_a, schema_b, &merged)?;
    require_narrowing_keys(&ast.domain, join, scope)?;
    let ast = resolve_params(ast, &merged)?;
    let plan = validate_resolved(&ast, &merged)?;
    gate_join(&ast, schema_a, schema_b)?;
    let cursor_domain = join_cursor_key(&ast.domain, &join.domain);
    let offset = decode_offset(cursor_text, &cursor_domain, scope)?;
    let outer_filter = ast
        .predicate
        .as_ref()
        .and_then(|predicate| leading_filter(predicate, schema_a));
    let mut outer = scope_filter(
        source.fetch(&ast.domain, scope, outer_filter.as_ref())?,
        scope,
        schema_a,
    );
    let order_by = resolved_order(&plan, merged.natural_order);
    let bound = page_bound(&ast, &order_by, schema_a, plan.limit, offset);
    // The join emits an outer row's matches together, so ordering the outer side first leaves the
    // merged sequence in its final order. Only then does stopping at a page's worth of rows keep
    // the rows the unbounded join would have paged.
    if bound.is_some() {
        order_source_rows(&mut outer, &order_by);
    }
    let probe = scope_filter(source.fetch(&join.domain, scope, None)?, scope, schema_b);
    let candidates = join_rows(
        outer,
        &probe,
        &join.on,
        schema_a,
        schema_b,
        bound,
        ast.predicate.as_ref(),
    )?;
    Ok(finish(
        candidates,
        &ast,
        plan,
        merged.natural_order,
        &cursor_domain,
        scope,
        offset,
    ))
}

/// The merged rows this page can read, or `None` when an aggregate or an order term the outer
/// domain does not carry reads every merged row, leaving the hard match cap as the only bound.
fn page_bound(ast: &Ast, order_by: &[OrderKey], outer: &DomainSchema, limit: u32, offset: u64) -> Option<usize> {
    if ast.aggregate.is_some() || order_by.iter().any(|key| outer.column(&key.field).is_none()) {
        return None;
    }
    // One row past the page is what mints the next cursor, and nothing downstream reads further.
    let needed = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .saturating_add(limit as usize)
        .saturating_add(1);
    Some(needed.min(MAX_JOIN_MATCHES))
}

/// A join narrows only when its keys can differ across probe rows. A domain joined to itself pairs
/// every row with every row sharing its key, and once the caller is pinned to their repositories
/// the scope column holds one of a handful of values on both sides, so either key set names a
/// product rather than a lookup.
fn require_narrowing_keys(domain: &str, join: &Join, scope: &QueryScope) -> Result<(), PqlError> {
    if domain == join.domain {
        return Err(PqlError::UnboundedJoin(format!(
            "`{domain}` joined to itself pairs its rows with each other, so the join cannot be bounded"
        )));
    }
    if join.on == [SCOPE_COLUMN] && matches!(scope.repositories(), RepoScope::Only(_)) {
        return Err(PqlError::UnboundedJoin(format!(
            "join key `{SCOPE_COLUMN}` already pins this query, so it cannot narrow `{}`",
            join.domain
        )));
    }
    Ok(())
}

/// `matching` has already dropped every row the predicate rejects, so a bounded join can count the
/// rows it keeps against the page it must fill.
fn finish(
    mut matching: Vec<Row>,
    ast: &Ast,
    plan: Plan,
    natural_order: &str,
    cursor_domain: &str,
    scope: &QueryScope,
    offset: u64,
) -> Page {
    let tuples = if let Some(aggregate) = &ast.aggregate {
        let mut tuples = aggregate_rows(&matching, aggregate, &plan.outputs);
        order_rows(&mut tuples, &plan.order_by, &plan.outputs);
        tuples
    } else {
        order_source_rows(&mut matching, &resolved_order(&plan, natural_order));
        project_rows(&matching, &plan.outputs)
    };
    paginate(tuples, plan, cursor_domain, scope, offset)
}

fn retain_matching(rows: Vec<Row>, predicate: Option<&Predicate>) -> Vec<Row> {
    rows.into_iter()
        .filter(|row| matches_predicate(row, predicate))
        .collect()
}

fn matches_predicate(row: &Row, predicate: Option<&Predicate>) -> bool {
    predicate.is_none_or(|predicate| evaluate(predicate, row))
}

fn decode_offset(cursor_text: Option<&str>, domain: &str, scope: &QueryScope) -> Result<u64, PqlError> {
    cursor_text.map_or(Ok(0), |text| cursor::decode(text, domain, scope))
}

fn scope_filter(rows: Vec<Row>, scope: &QueryScope, schema: &DomainSchema) -> Vec<Row> {
    let has_scope_column = schema.column(SCOPE_COLUMN).is_some();
    rows.into_iter()
        .filter(|row| in_scope(row, scope, has_scope_column))
        .collect()
}

fn validate_join(
    keys: &[String],
    outer: &DomainSchema,
    probe: &DomainSchema,
    visible: &DomainSchema,
) -> Result<(), PqlError> {
    for key in keys {
        let Some(outer_column) = outer.column(key) else {
            return Err(PqlError::Validation(format!(
                "join key `{key}` is not a column of `{}`",
                outer.name
            )));
        };
        let Some(probe_column) = probe.column(key) else {
            return Err(PqlError::Validation(format!(
                "join key `{key}` is not a column of `{}`",
                probe.name
            )));
        };
        // Row presence after an inner join discloses the key's value, so a key the caller cannot
        // read is unknown to them, exactly as it is in every other clause.
        require_column(key, visible)?;
        if outer_column.value_type != probe_column.value_type {
            return Err(PqlError::Validation(format!(
                "join key `{key}` type differs: `{}` is `{}`, `{}` is `{}`",
                outer.name,
                outer_column.value_type.as_str(),
                probe.name,
                probe_column.value_type.as_str()
            )));
        }
        if !probe_column.indexability.is_cheap() {
            return Err(PqlError::UnboundedJoin(format!(
                "`{}` has no index on join key `{key}`, so the join cannot be bounded",
                probe.name
            )));
        }
    }
    Ok(())
}

fn merge_schemas(outer: &DomainSchema, probe: &DomainSchema) -> DomainSchema {
    let mut columns = outer.columns.clone();
    for column in &probe.columns {
        if let Some(shared) = columns.iter_mut().find(|existing| existing.name == column.name) {
            shared.class = shared.class.most_restrictive(column.class);
        } else {
            columns.push(column.clone());
        }
    }
    DomainSchema {
        name: outer.name,
        columns,
        auth: outer.auth,
        natural_order: outer.natural_order,
        bounded: outer.bounded && probe.bounded,
        pushdown: outer.pushdown,
    }
}

/// Merges at most `bound` rows that satisfy `predicate`, and refuses a join walking more than
/// [`MAX_JOIN_MATCHES`] key matches, so neither the kept rows nor the work to find them grows with
/// the product of the two domains.
///
/// # Errors
/// Returns [`PqlError::CostExceeded`] once the match cap is crossed.
fn join_rows(
    outer: Vec<Row>,
    probe: &[Row],
    keys: &[String],
    schema_outer: &DomainSchema,
    schema_probe: &DomainSchema,
    bound: Option<usize>,
    predicate: Option<&Predicate>,
) -> Result<Vec<Row>, PqlError> {
    let probe_only: Vec<&'static str> = schema_probe
        .columns
        .iter()
        .filter(|column| schema_outer.column(column.name).is_none())
        .map(|column| column.name)
        .collect();
    let mut index: HashMap<String, Vec<&Row>> = HashMap::new();
    for row in probe {
        index.entry(join_key(row, keys)).or_default().push(row);
    }
    let mut merged = Vec::new();
    let mut walked = 0usize;
    'outer: for row in outer {
        let Some(matches) = index.get(&join_key(&row, keys)) else {
            continue;
        };
        for probe_row in matches {
            walked += 1;
            if walked > MAX_JOIN_MATCHES {
                return Err(PqlError::CostExceeded(format!(
                    "joining `{}` to `{}` matches more than {MAX_JOIN_MATCHES} row pairs; add a join key that narrows it",
                    schema_outer.name, schema_probe.name
                )));
            }
            let candidate = merge_row(&row, probe_row, &probe_only);
            if !matches_predicate(&candidate, predicate) {
                continue;
            }
            merged.push(candidate);
            if bound == Some(merged.len()) {
                break 'outer;
            }
        }
    }
    Ok(merged)
}

fn merge_row(outer: &Row, probe: &Row, probe_only: &[&'static str]) -> Row {
    let mut row = outer.clone();
    for name in probe_only {
        row = row.with(name, probe.get(name));
    }
    row
}

/// Debug forms keep unlike typed values from sharing a join key.
fn join_key(row: &Row, keys: &[String]) -> String {
    use std::fmt::Write as _;
    let mut key = String::new();
    for name in keys {
        let _ = write!(key, "{:?}\u{1}", row.get(name));
    }
    key
}

fn join_cursor_key(outer: &str, probe: &str) -> String {
    format!("{outer}\u{1}{probe}")
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
                    // Aggregates saturate so release builds cannot wrap into negative totals.
                    *total = total.saturating_add(number);
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
    vec![OrderKey {
        field: natural_order.to_owned(),
        descending: true,
    }]
}

fn order_source_rows(rows: &mut [Row], order_by: &[OrderKey]) {
    rows.sort_by(|left, right| compare_source_rows(left, right, order_by));
}

fn compare_source_rows(left: &Row, right: &Row, order_by: &[OrderKey]) -> Ordering {
    for key in order_by {
        let order = left
            .get(&key.field)
            .compare(&right.get(&key.field))
            .unwrap_or(Ordering::Equal);
        let order = if key.descending { order.reverse() } else { order };
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
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
