use std::collections::BTreeMap;

use crate::error::PqlError;
use crate::execute::{Page, execute};
use crate::parse::parse;
use crate::source::FetchFilter;
use crate::value::{Row, Value};
use crate::{QueryScope, run};

use super::support::{TestSource, decision, operator_scope, repository_scope};

fn rows() -> Vec<Row> {
    vec![
        decision("alpha", "numpy", "blocked", "cache", 300, 10),
        decision("alpha", "scipy", "allowed", "origin", 200, 5),
        decision("alpha", "flask", "blocked", "cache", 100, 7),
        decision("other", "django", "blocked", "origin", 250, 3),
        // A row missing `downloads`, so aggregation exercises the skip-null path.
        Row::new()
            .with("repository", Value::Str("alpha".to_owned()))
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
    let page = query("from policy.decisions", &repository_scope("alpha"), None).expect("runs");
    assert_eq!(projects(&page), ["numpy", "scipy", "toolz", "flask"]);
    assert!(!projects(&page).contains(&"django".to_owned()));
}

#[test]
fn test_execute_scope_drops_row_without_string_repository() {
    // Scope injection permits a row only when its `repository` cell is a string it is allowed to read.
    // A row missing the scope column has a non-string value there, so a repository-scoped read must
    // exclude it rather than leak it.
    let rows = vec![
        decision("alpha", "numpy", "allowed", "cache", 100, 1),
        Row::new()
            .with("project", Value::Str("ghost".to_owned()))
            .with("state", Value::Str("allowed".to_owned()))
            .with("source", Value::Str("cache".to_owned()))
            .with("evaluated_at", Value::Timestamp(50)),
    ];
    let page = execute(
        &parse("from policy.decisions select project").expect("parses"),
        &repository_scope("alpha"),
        None,
        &TestSource::new(rows),
    )
    .expect("runs");
    assert_eq!(projects(&page), ["numpy"]);
}

#[test]
fn test_execute_order_by_tied_key_keeps_both_rows() {
    // Two rows share the ordering key, so the tuple comparison exhausts every key and falls through to
    // `Equal`. The tie must keep both rows in their original, stable order.
    let rows = vec![
        decision("alpha", "numpy", "blocked", "cache", 300, 10),
        decision("alpha", "scipy", "blocked", "cache", 200, 5),
    ];
    let page = execute(
        &parse("from policy.decisions select project, state order by state asc").expect("parses"),
        &operator_scope(),
        None,
        &TestSource::new(rows),
    )
    .expect("runs");
    assert_eq!(projects(&page), ["numpy", "scipy"]);
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
        &repository_scope("alpha"),
        None,
    )
    .expect("runs");
    // The row missing downloads is excluded by the predicate; the rest ascend by download count.
    assert_eq!(projects(&page), ["scipy", "flask", "numpy"]);
}

#[test]
fn test_execute_subset_without_natural_order_keeps_source_order() {
    let page = query("from policy.decisions select project", &repository_scope("alpha"), None).expect("runs");
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
    let first = query("from policy.decisions limit 2", &repository_scope("alpha"), None).expect("runs");
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
        &repository_scope("alpha"),
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
fn test_execute_sum_saturates_instead_of_wrapping() {
    let rows = vec![
        decision("alpha", "a", "allowed", "cache", 10, i64::MAX),
        decision("alpha", "b", "allowed", "cache", 20, i64::MAX),
    ];
    let page = execute(
        &parse("from policy.decisions aggregate sum(downloads) as total by state").expect("parses"),
        &operator_scope(),
        None,
        &TestSource::new(rows),
    )
    .expect("runs");
    // Two i64::MAX downloads would wrap to a negative under an unchecked add; saturating pins the
    // sum at the ceiling instead.
    assert_eq!(page.outputs[1].name, "total");
    assert_eq!(page.rows[0][1], Value::Int(i64::MAX));
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
            ("repo".to_owned(), Value::Str("alpha".to_owned())),
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

#[test]
fn test_execute_matches_non_ascii_string_literal() {
    // A multibyte literal must survive lexing as one codepoint so it equals a multibyte field value;
    // before the UTF-8 fix "café" lexed to "cafÃ©" and matched nothing.
    let rows = vec![
        decision("alpha", "café", "allowed", "cache", 10, 1),
        decision("alpha", "resumé", "allowed", "cache", 20, 2),
    ];
    let page = execute(
        &parse(r#"from policy.decisions where project == "café" select project"#).expect("parses"),
        &operator_scope(),
        None,
        &TestSource::new(rows),
    )
    .expect("runs");
    assert_eq!(projects(&page), ["café"]);
}

#[test]
fn test_execute_leading_filter_reaches_source() {
    // The cost gate admits `big` only for an indexed leading equality; that same filter must arrive at
    // the source so an unbounded domain is narrowed through its index, not materialized whole.
    let source = TestSource::new(rows());
    execute(
        &parse(r#"from big where repository == "alpha""#).expect("parses"),
        &operator_scope(),
        None,
        &source,
    )
    .expect("runs");
    assert_eq!(
        source.fetches(),
        vec![(
            "big".to_owned(),
            Some(FetchFilter {
                column: "repository",
                values: vec![Value::Str("alpha".to_owned())],
            })
        )]
    );
}

#[test]
fn test_execute_omits_filter_without_cheap_leading_equality() {
    let source = TestSource::new(rows());
    execute(
        &parse(r#"from policy.decisions where state == "blocked""#).expect("parses"),
        &operator_scope(),
        None,
        &source,
    )
    .expect("runs");
    assert_eq!(source.fetches(), vec![("policy.decisions".to_owned(), None)]);
}

fn cell(page: &Page, project: &str, column: &str) -> Value {
    let project_index = page
        .outputs
        .iter()
        .position(|output| output.name == "project")
        .expect("project");
    let column_index = page
        .outputs
        .iter()
        .position(|output| output.name == column)
        .expect("column");
    page.rows
        .iter()
        .find(|row| row[project_index] == Value::Str(project.to_owned()))
        .map(|row| row[column_index].clone())
        .expect("row for project")
}

#[test]
fn test_execute_join_matches_on_composite_key() {
    // Inner join on (repository, project): only projects with a usage row survive; flask and toolz
    // have none. usage brings in `hits` and `bytes`.
    let page = query(
        "from policy.decisions join usage on repository, project order by evaluated_at desc",
        &operator_scope(),
        None,
    )
    .expect("runs");
    assert_eq!(projects(&page), ["numpy", "django", "scipy"]);
    assert_eq!(cell(&page, "numpy", "hits"), Value::Int(100));
    assert_eq!(cell(&page, "django", "bytes"), Value::Int(3));
}

#[test]
fn test_execute_join_scopes_both_sides() {
    let page = query(
        "from policy.decisions join usage on repository, project",
        &repository_scope("alpha"),
        None,
    )
    .expect("runs");
    assert_eq!(projects(&page).len(), 2);
    assert!(!projects(&page).contains(&"django".to_owned()));
}

#[test]
fn test_execute_join_filters_on_probe_column() {
    let page = query(
        "from policy.decisions join usage on repository, project where hits >= 60",
        &operator_scope(),
        None,
    )
    .expect("runs");
    assert_eq!(projects(&page), ["numpy"]);
}

#[test]
fn test_execute_join_selects_columns_from_both_domains() {
    let page = query(
        "from policy.decisions join usage on repository, project select project, state, hits",
        &repository_scope("alpha"),
        None,
    )
    .expect("runs");
    assert_eq!(
        page.outputs
            .iter()
            .map(|output| output.name.as_str())
            .collect::<Vec<_>>(),
        ["project", "state", "hits"]
    );
}

#[test]
fn test_execute_join_rejects_unindexed_probe_key() {
    let refused = query(
        "from policy.decisions join usage_scan on repository, project",
        &operator_scope(),
        None,
    );
    assert!(matches!(refused, Err(PqlError::UnboundedJoin(_))));
}

#[test]
fn test_execute_join_rejects_unknown_key() {
    let outer = query(
        "from policy.decisions join usage on repository, missing",
        &operator_scope(),
        None,
    );
    assert!(matches!(outer, Err(PqlError::Validation(_))));
    let probe = query("from policy.decisions join usage on state", &operator_scope(), None);
    assert!(matches!(probe, Err(PqlError::Validation(_))));
}

#[test]
fn test_execute_join_unknown_probe_domain_is_not_disclosed() {
    let result = query(
        "from policy.decisions join ghosts on repository, project",
        &operator_scope(),
        None,
    );
    assert_eq!(result, Err(PqlError::Unauthorized));
}

#[test]
fn test_execute_join_cursor_is_distinct_and_scope_bound() {
    let scope = operator_scope();
    let first = query(
        "from policy.decisions join usage on repository, project limit 1",
        &scope,
        None,
    )
    .expect("runs");
    let cursor = first.next_cursor.expect("join paginates");

    // The join cursor names the joined pair, so a single-domain query rejects it as malformed.
    assert_eq!(
        query("from policy.decisions limit 1", &scope, Some(&cursor)),
        Err(PqlError::InvalidCursor)
    );
    // A different grant refuses the replay.
    assert_eq!(
        query(
            "from policy.decisions join usage on repository, project limit 1",
            &repository_scope("alpha"),
            Some(&cursor)
        ),
        Err(PqlError::CursorScopeChanged)
    );
    // The same scope resumes.
    let second = query(
        "from policy.decisions join usage on repository, project limit 1",
        &scope,
        Some(&cursor),
    )
    .expect("resumes");
    assert_eq!(second.rows.len(), 1);
}

#[test]
fn test_execute_join_aggregates_probe_metric() {
    let page = query(
        "from policy.decisions join usage on repository, project aggregate sum(hits) as total by repository",
        &operator_scope(),
        None,
    )
    .expect("runs");
    let repository_index = page
        .outputs
        .iter()
        .position(|output| output.name == "repository")
        .unwrap();
    let total_index = page.outputs.iter().position(|output| output.name == "total").unwrap();
    let totals: BTreeMap<String, i64> = page
        .rows
        .iter()
        .map(|row| {
            let Value::Str(repository) = &row[repository_index] else {
                panic!("repository")
            };
            let Value::Int(total) = &row[total_index] else {
                panic!("total")
            };
            (repository.clone(), *total)
        })
        .collect();
    assert_eq!(totals["alpha"], 150);
    assert_eq!(totals["other"], 30);
}

#[test]
fn test_execute_join_rejects_unbounded_probe_domain() {
    // `big` is unbounded, so materializing it as the build side is refused even though its join key is
    // key-ordered: the executor indexes the whole probe domain, not a per-key slice.
    let refused = query("from policy.decisions join big on repository", &operator_scope(), None);
    assert!(matches!(refused, Err(PqlError::UnboundedJoin(_))));
}

#[test]
fn test_execute_join_rejects_unbounded_outer_without_leading_filter() {
    // The outer side is streamed, so an unbounded outer with no cheap leading filter is over budget,
    // the same as a single-domain scan of it would be.
    let refused = query("from big join policy.decisions on repository", &operator_scope(), None);
    assert!(matches!(refused, Err(PqlError::CostExceeded(_))));
}

#[test]
fn test_execute_join_admits_bounded_outer_with_leading_filter() {
    // A bounded probe and an outer narrowed by an indexed equality is affordable and runs.
    let page = query(
        r#"from big join policy.decisions on repository where repository == "alpha""#,
        &operator_scope(),
        None,
    )
    .expect("runs");
    assert!(!page.rows.is_empty());
}

#[test]
fn test_execute_join_refuses_an_empty_key_set() {
    // The text parser guarantees a key; build the join a JSON-AST front-end could produce instead by
    // clearing the keys after parsing. An empty key set would cross-product every row.
    let mut ast = parse(r#"from big join policy.decisions on repository where repository == "alpha""#).expect("parses");
    ast.join.as_mut().expect("has a join").on.clear();

    let result = execute(&ast, &operator_scope(), None, &TestSource::new(rows()));

    assert!(matches!(result, Err(PqlError::Validation(_))));
}
