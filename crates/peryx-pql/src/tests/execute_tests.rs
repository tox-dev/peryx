use std::collections::BTreeMap;

use crate::error::PqlError;
use crate::execute::{Page, execute};
use crate::parse::parse;
use crate::value::{Row, Value};
use crate::{QueryScope, run};

use super::support::{TestSource, decision, operator_scope, repository_scope};

fn rows() -> Vec<Row> {
    vec![
        decision("pypi", "numpy", "blocked", "cache", 300, 10),
        decision("pypi", "scipy", "allowed", "origin", 200, 5),
        decision("pypi", "flask", "blocked", "cache", 100, 7),
        decision("other", "django", "blocked", "origin", 250, 3),
        // A row missing `downloads`, so aggregation exercises the skip-null path.
        Row::new()
            .with("repository", Value::Str("pypi".to_owned()))
            .with("project", Value::Str("toolz".to_owned()))
            .with("state", Value::Str("allowed".to_owned()))
            .with("source", Value::Str("cache".to_owned()))
            .with("evaluated_at", Value::Timestamp(150)),
    ]
}

fn query(text: &str, scope: &QueryScope, cursor: Option<&str>) -> Result<Page, PqlError> {
    execute(&parse(text).expect("parses"), scope, cursor, &TestSource::new(rows()))
}

fn projects(page: &Page) -> Vec<String> {
    let index = page
        .outputs
        .iter()
        .position(|output| output.name == "project")
        .expect("project column");
    page.rows
        .iter()
        .map(|row| match &row[index] {
            Value::Str(name) => name.clone(),
            other => panic!("expected a string project, got {other:?}"),
        })
        .collect()
}

#[test]
fn test_execute_orders_by_natural_key_desc() {
    let page = query("from policy.decisions", &operator_scope(), None).expect("runs");
    assert_eq!(projects(&page), ["numpy", "django", "scipy", "toolz", "flask"]);
}

#[test]
fn test_execute_injects_repository_scope() {
    let page = query("from policy.decisions", &repository_scope("pypi"), None).expect("runs");
    assert_eq!(projects(&page), ["numpy", "scipy", "toolz", "flask"]);
    assert!(!projects(&page).contains(&"django".to_owned()));
}

#[test]
fn test_execute_applies_user_predicate_after_scope() {
    let page = query(
        r#"from policy.decisions where state == "blocked""#,
        &operator_scope(),
        None,
    )
    .expect("runs");
    assert_eq!(projects(&page), ["numpy", "django", "flask"]);
}

#[test]
fn test_execute_explicit_order() {
    let page = query(
        "from policy.decisions where downloads >= 0 select project, downloads order by downloads asc",
        &repository_scope("pypi"),
        None,
    )
    .expect("runs");
    // The row missing downloads is excluded by the predicate; the rest ascend by download count.
    assert_eq!(projects(&page), ["scipy", "flask", "numpy"]);
}

#[test]
fn test_execute_subset_without_natural_order_keeps_source_order() {
    let page = query("from policy.decisions select project", &repository_scope("pypi"), None).expect("runs");
    assert_eq!(projects(&page), ["numpy", "scipy", "flask", "toolz"]);
}

#[test]
fn test_execute_paginates_with_scope_bound_cursor() {
    let scope = operator_scope();
    let first = query("from policy.decisions limit 2", &scope, None).expect("runs");
    assert_eq!(projects(&first), ["numpy", "django"]);
    let cursor = first.next_cursor.expect("has next page");

    let second = query("from policy.decisions limit 2", &scope, Some(&cursor)).expect("runs");
    assert_eq!(projects(&second), ["scipy", "toolz"]);
    let cursor = second.next_cursor.expect("has next page");

    let third = query("from policy.decisions limit 2", &scope, Some(&cursor)).expect("runs");
    assert_eq!(projects(&third), ["flask"]);
    assert!(third.next_cursor.is_none());
}

#[test]
fn test_execute_rejects_replayed_cursor_after_scope_change() {
    let first = query("from policy.decisions limit 2", &repository_scope("pypi"), None).expect("runs");
    let cursor = first.next_cursor.expect("has next page");
    assert_eq!(
        query(
            "from policy.decisions limit 2",
            &repository_scope("other"),
            Some(&cursor)
        ),
        Err(PqlError::CursorScopeChanged)
    );
}

#[test]
fn test_execute_unknown_domain_is_not_disclosed() {
    assert_eq!(
        query("from ghosts", &operator_scope(), None),
        Err(PqlError::Unauthorized)
    );
}

#[test]
fn test_execute_backend_failure_propagates() {
    let source = TestSource::failing();
    let result = execute(
        &parse("from policy.decisions").expect("parses"),
        &operator_scope(),
        None,
        &source,
    );
    assert!(matches!(result, Err(PqlError::Backend(_))));
}

#[test]
fn test_execute_keyless_domain_needs_no_repository() {
    let source = TestSource::new(Vec::new());
    let page = execute(&parse("from notes").expect("parses"), &operator_scope(), None, &source).expect("runs");
    let ids: Vec<Value> = page.rows.iter().map(|row| row[0].clone()).collect();
    assert_eq!(ids, [Value::Int(2), Value::Int(1)]);
}

#[test]
fn test_execute_count_and_sum_aggregate() {
    let page = query(
        "from policy.decisions aggregate count() as n, sum(downloads) as total by state",
        &operator_scope(),
        None,
    )
    .expect("runs");
    let mut grouped: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for row in &page.rows {
        let Value::Str(state) = &row[0] else { panic!("state") };
        let (Value::Int(n), Value::Int(total)) = (&row[1], &row[2]) else {
            panic!("aggregates are integers")
        };
        grouped.insert(state.clone(), (*n, *total));
    }
    assert_eq!(grouped["blocked"], (3, 20));
    assert_eq!(grouped["allowed"], (2, 5));
}

#[test]
fn test_execute_min_max_aggregate_over_missing_values() {
    let page = query(
        "from policy.decisions aggregate min(downloads) as lo, max(downloads) as hi by state",
        &repository_scope("pypi"),
        None,
    )
    .expect("runs");
    let mut grouped: BTreeMap<String, (Value, Value)> = BTreeMap::new();
    for row in &page.rows {
        let Value::Str(state) = &row[0] else { panic!("state") };
        grouped.insert(state.clone(), (row[1].clone(), row[2].clone()));
    }
    assert_eq!(grouped["blocked"], (Value::Int(7), Value::Int(10)));
    // toolz has no downloads, so `allowed` reduces to scipy's single value.
    assert_eq!(grouped["allowed"], (Value::Int(5), Value::Int(5)));
}

#[test]
fn test_execute_min_timestamp_aggregate() {
    let page = query(
        "from policy.decisions aggregate min(evaluated_at) as first by state",
        &operator_scope(),
        None,
    )
    .expect("runs");
    let first_blocked = page
        .rows
        .iter()
        .find(|row| row[0] == Value::Str("blocked".to_owned()))
        .map(|row| row[1].clone());
    assert_eq!(first_blocked, Some(Value::Timestamp(100)));
}

#[test]
fn test_run_end_to_end_binds_parameters() {
    let page = run(
        "from policy.decisions where repository == :repo and state == :state order by evaluated_at desc",
        &[
            ("repo".to_owned(), Value::Str("pypi".to_owned())),
            ("state".to_owned(), Value::Str("blocked".to_owned())),
        ]
        .into_iter()
        .collect(),
        &operator_scope(),
        None,
        &TestSource::new(rows()),
    )
    .expect("runs");
    assert_eq!(projects(&page), ["numpy", "flask"]);
}

#[test]
fn test_run_surfaces_parse_error() {
    let result = run(
        "nonsense",
        &BTreeMap::new(),
        &operator_scope(),
        None,
        &TestSource::new(rows()),
    );
    assert!(matches!(result, Err(PqlError::Parse(_))));
}
