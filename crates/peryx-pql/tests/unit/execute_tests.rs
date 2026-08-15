use crate::catalog::{Column, DomainAuth, DomainSchema, FieldClass, Indexability};
use crate::cursor;
use crate::error::PqlError;
use crate::execute::{Page, execute};
use crate::parse::parse;
use crate::plan::OutputColumn;
use crate::source::{DataSource, FetchFilter};
use crate::value::{Row, Value, ValueType};
use crate::{QueryScope, run};
use rstest::rstest;

use super::support::{TestSource, decision, operator_scope, repository_scope};

fn rows() -> Vec<Row> {
    vec![
        decision("alpha", "resource-a", "blocked", "cache", 300, 10),
        decision("alpha", "resource-b", "allowed", "origin", 200, 5),
        decision("alpha", "resource-c", "blocked", "cache", 100, 7),
        decision("other", "resource-d", "blocked", "origin", 250, 3),
        Row::new()
            .with("repository", Value::Str("alpha".to_owned()))
            .with("resource", Value::Str("resource-e".to_owned()))
            .with("state", Value::Str("allowed".to_owned()))
            .with("source", Value::Str("cache".to_owned()))
            .with("evaluated_at", Value::Timestamp(150)),
    ]
}

fn query(text: &str, scope: &QueryScope, cursor: Option<&str>) -> Result<Page, PqlError> {
    execute(&parse(text).expect("parses"), scope, cursor, &TestSource::new(rows()))
}

#[test]
fn test_execute_orders_by_natural_key_desc() {
    assert_eq!(
        query(
            "from policy.decisions select resource, evaluated_at",
            &operator_scope(),
            None
        ),
        Ok(resource_time_page(
            &[
                ("resource-a", 300),
                ("resource-d", 250),
                ("resource-b", 200),
                ("resource-e", 150),
                ("resource-c", 100),
            ],
            None,
        ))
    );
}

#[test]
fn test_execute_injects_repository_scope() {
    assert_eq!(
        query(
            "from policy.decisions select resource, evaluated_at",
            &repository_scope("alpha"),
            None,
        ),
        Ok(resource_time_page(
            &[
                ("resource-a", 300),
                ("resource-b", 200),
                ("resource-e", 150),
                ("resource-c", 100),
            ],
            None,
        ))
    );
}

#[test]
fn test_execute_scope_drops_row_without_string_repository() {
    let rows = vec![
        decision("alpha", "resource-a", "allowed", "cache", 100, 1),
        Row::new()
            .with("resource", Value::Str("ghost".to_owned()))
            .with("state", Value::Str("allowed".to_owned()))
            .with("source", Value::Str("cache".to_owned()))
            .with("evaluated_at", Value::Timestamp(50)),
    ];
    let page = execute(
        &parse("from policy.decisions select resource").expect("parses"),
        &repository_scope("alpha"),
        None,
        &TestSource::new(rows),
    )
    .expect("runs");
    assert_eq!(page, resource_page(&["resource-a"], None));
}

#[test]
fn test_execute_order_by_tied_key_keeps_both_rows() {
    let rows = vec![
        decision("alpha", "resource-a", "blocked", "cache", 300, 10),
        decision("alpha", "resource-b", "blocked", "cache", 200, 5),
    ];
    let page = execute(
        &parse("from policy.decisions select resource, state order by state asc").expect("parses"),
        &operator_scope(),
        None,
        &TestSource::new(rows),
    )
    .expect("runs");
    assert_eq!(
        page,
        Page {
            outputs: vec![
                output("resource", FieldClass::Repository, ValueType::Str),
                output("state", FieldClass::Repository, ValueType::Str),
            ],
            rows: vec![
                vec![Value::Str("resource-a".to_owned()), Value::Str("blocked".to_owned())],
                vec![Value::Str("resource-b".to_owned()), Value::Str("blocked".to_owned())],
            ],
            next_cursor: None,
        }
    );
}

#[test]
fn test_execute_applies_user_predicate_after_scope() {
    let page = query(
        r#"from policy.decisions where state == "blocked" select resource, evaluated_at"#,
        &operator_scope(),
        None,
    )
    .expect("runs");
    assert_eq!(
        page,
        resource_time_page(&[("resource-a", 300), ("resource-d", 250), ("resource-c", 100)], None)
    );
}

#[test]
fn test_execute_explicit_order() {
    let page = query(
        "from policy.decisions where reads >= 0 select resource, reads order by reads asc",
        &repository_scope("alpha"),
        None,
    )
    .expect("runs");
    assert_eq!(
        page,
        Page {
            outputs: vec![
                output("resource", FieldClass::Repository, ValueType::Str),
                output("reads", FieldClass::Repository, ValueType::Int),
            ],
            rows: vec![
                vec![Value::Str("resource-b".to_owned()), Value::Int(5)],
                vec![Value::Str("resource-c".to_owned()), Value::Int(7)],
                vec![Value::Str("resource-a".to_owned()), Value::Int(10)],
            ],
            next_cursor: None,
        }
    );
}

#[test]
fn test_execute_subset_without_natural_order_keeps_source_order() {
    let page = query(
        "from policy.decisions select resource",
        &repository_scope("alpha"),
        None,
    )
    .expect("runs");
    assert_eq!(
        page,
        resource_page(&["resource-a", "resource-b", "resource-c", "resource-e"], None)
    );
}

#[test]
fn test_execute_paginates_with_scope_bound_cursor() {
    let scope = operator_scope();
    let text = "from policy.decisions select resource, evaluated_at limit 2";
    let cursor = cursor::encode("policy.decisions", &scope, 2);
    let first = query(text, &scope, None).expect("runs");
    assert_eq!(
        first,
        resource_time_page(&[("resource-a", 300), ("resource-d", 250)], Some(cursor.clone()))
    );

    let next_cursor = cursor::encode("policy.decisions", &scope, 4);
    let second = query(text, &scope, Some(&cursor)).expect("runs");
    assert_eq!(
        second,
        resource_time_page(&[("resource-b", 200), ("resource-e", 150)], Some(next_cursor.clone()))
    );

    assert_eq!(
        query(text, &scope, Some(&next_cursor)),
        Ok(resource_time_page(&[("resource-c", 100)], None))
    );
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
    assert_eq!(result, Err(PqlError::Backend("store down".to_owned())));
}

#[test]
fn test_execute_keyless_domain_needs_no_repository() {
    let source = TestSource::new(Vec::new());
    assert_eq!(
        execute(&parse("from notes").expect("parses"), &operator_scope(), None, &source),
        Ok(Page {
            outputs: vec![
                output("id", FieldClass::Operator, ValueType::Int),
                output("body", FieldClass::Operator, ValueType::Str),
            ],
            rows: vec![
                vec![Value::Int(2), Value::Str("two".to_owned())],
                vec![Value::Int(1), Value::Str("one".to_owned())],
            ],
            next_cursor: None,
        })
    );
}

#[test]
fn test_execute_count_and_sum_aggregate() {
    let page = query(
        "from policy.decisions aggregate count() as n, sum(reads) as total by state order by state asc",
        &operator_scope(),
        None,
    )
    .expect("runs");
    assert_eq!(
        page,
        Page {
            outputs: vec![
                output("state", FieldClass::Repository, ValueType::Str),
                output("n", FieldClass::Public, ValueType::Int),
                output("total", FieldClass::Repository, ValueType::Int),
            ],
            rows: vec![
                vec![Value::Str("allowed".to_owned()), Value::Int(2), Value::Int(5)],
                vec![Value::Str("blocked".to_owned()), Value::Int(3), Value::Int(20)],
            ],
            next_cursor: None,
        }
    );
}

#[test]
fn test_execute_min_max_aggregate_over_missing_values() {
    let page = query(
        "from policy.decisions aggregate min(reads) as lo, max(reads) as hi by state order by state asc",
        &repository_scope("alpha"),
        None,
    )
    .expect("runs");
    assert_eq!(
        page,
        Page {
            outputs: vec![
                output("state", FieldClass::Repository, ValueType::Str),
                output("lo", FieldClass::Repository, ValueType::Int),
                output("hi", FieldClass::Repository, ValueType::Int),
            ],
            rows: vec![
                vec![Value::Str("allowed".to_owned()), Value::Int(5), Value::Int(5)],
                vec![Value::Str("blocked".to_owned()), Value::Int(7), Value::Int(10)],
            ],
            next_cursor: None,
        }
    );
}

#[test]
fn test_execute_sum_saturates_instead_of_wrapping() {
    let rows = vec![
        decision("alpha", "a", "allowed", "cache", 10, i64::MAX),
        decision("alpha", "b", "allowed", "cache", 20, i64::MAX),
    ];
    let page = execute(
        &parse("from policy.decisions aggregate sum(reads) as total by state").expect("parses"),
        &operator_scope(),
        None,
        &TestSource::new(rows),
    )
    .expect("runs");
    assert_eq!(
        page,
        Page {
            outputs: vec![
                output("state", FieldClass::Repository, ValueType::Str),
                output("total", FieldClass::Repository, ValueType::Int),
            ],
            rows: vec![vec![Value::Str("allowed".to_owned()), Value::Int(i64::MAX)]],
            next_cursor: None,
        }
    );
}

#[test]
fn test_execute_min_timestamp_aggregate() {
    let page = query(
        "from policy.decisions aggregate min(evaluated_at) as first by state order by state asc",
        &operator_scope(),
        None,
    )
    .expect("runs");
    assert_eq!(
        page,
        Page {
            outputs: vec![
                output("state", FieldClass::Repository, ValueType::Str),
                output("first", FieldClass::Repository, ValueType::Timestamp),
            ],
            rows: vec![
                vec![Value::Str("allowed".to_owned()), Value::Timestamp(150)],
                vec![Value::Str("blocked".to_owned()), Value::Timestamp(100)],
            ],
            next_cursor: None,
        }
    );
}

#[test]
fn test_run_end_to_end_binds_parameters() {
    let page = run(
        "from policy.decisions where repository == :repo and state == :state select resource, evaluated_at order by evaluated_at desc",
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
    assert_eq!(
        page,
        resource_time_page(&[("resource-a", 300), ("resource-c", 100)], None)
    );
}

#[test]
fn test_run_surfaces_parse_error() {
    use std::collections::BTreeMap;

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
    let rows = vec![
        decision("alpha", "café", "allowed", "cache", 10, 1),
        decision("alpha", "resumé", "allowed", "cache", 20, 2),
    ];
    let page = execute(
        &parse(r#"from policy.decisions where resource == "café" select resource"#).expect("parses"),
        &operator_scope(),
        None,
        &TestSource::new(rows),
    )
    .expect("runs");
    assert_eq!(page, resource_page(&["café"], None));
}

#[test]
fn test_execute_leading_filter_reaches_source() {
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

#[test]
fn test_execute_join_matches_on_composite_key() {
    let page = query(
        "from policy.decisions join usage on repository, resource select resource, hits, bytes, evaluated_at order by evaluated_at desc",
        &operator_scope(),
        None,
    )
    .expect("runs");
    assert_eq!(
        page,
        Page {
            outputs: vec![
                output("resource", FieldClass::Repository, ValueType::Str),
                output("hits", FieldClass::Repository, ValueType::Int),
                output("bytes", FieldClass::Operator, ValueType::Int),
                output("evaluated_at", FieldClass::Repository, ValueType::Timestamp),
            ],
            rows: vec![
                vec![
                    Value::Str("resource-a".to_owned()),
                    Value::Int(100),
                    Value::Int(10),
                    Value::Timestamp(300),
                ],
                vec![
                    Value::Str("resource-d".to_owned()),
                    Value::Int(30),
                    Value::Int(3),
                    Value::Timestamp(250),
                ],
                vec![
                    Value::Str("resource-b".to_owned()),
                    Value::Int(50),
                    Value::Int(5),
                    Value::Timestamp(200),
                ],
            ],
            next_cursor: None,
        }
    );
}

#[test]
fn test_execute_join_scopes_both_sides() {
    let page = query(
        "from policy.decisions join usage on repository, resource select resource",
        &repository_scope("alpha"),
        None,
    )
    .expect("runs");
    assert_eq!(page, resource_page(&["resource-a", "resource-b"], None));
}

#[test]
fn test_execute_join_filters_on_probe_column() {
    let page = query(
        "from policy.decisions join usage on repository, resource where hits >= 60 select resource",
        &operator_scope(),
        None,
    )
    .expect("runs");
    assert_eq!(page, resource_page(&["resource-a"], None));
}

#[test]
fn test_execute_join_selects_columns_from_both_domains() {
    let page = query(
        "from policy.decisions join usage on repository, resource select resource, state, hits",
        &repository_scope("alpha"),
        None,
    )
    .expect("runs");
    assert_eq!(
        page,
        Page {
            outputs: vec![
                output("resource", FieldClass::Repository, ValueType::Str),
                output("state", FieldClass::Repository, ValueType::Str),
                output("hits", FieldClass::Repository, ValueType::Int),
            ],
            rows: vec![
                vec![
                    Value::Str("resource-a".to_owned()),
                    Value::Str("blocked".to_owned()),
                    Value::Int(100),
                ],
                vec![
                    Value::Str("resource-b".to_owned()),
                    Value::Str("allowed".to_owned()),
                    Value::Int(50),
                ],
            ],
            next_cursor: None,
        }
    );
}

#[test]
fn test_execute_join_rejects_unindexed_probe_key() {
    let refused = query(
        "from policy.decisions join usage_scan on repository, resource",
        &operator_scope(),
        None,
    );
    assert!(matches!(refused, Err(PqlError::UnboundedJoin(_))));
}

#[rstest]
#[case::outer(
    "from policy.decisions join usage on repository, missing",
    "join key `missing` is not a column of `policy.decisions`"
)]
#[case::probe(
    "from policy.decisions join usage on state",
    "join key `state` is not a column of `usage`"
)]
fn test_execute_join_rejects_unknown_key(#[case] text: &str, #[case] error: &str) {
    assert_eq!(
        query(text, &operator_scope(), None),
        Err(PqlError::Validation(error.to_owned()))
    );
}

#[rstest]
#[case::bool(ValueType::Bool)]
#[case::int(ValueType::Int)]
#[case::str(ValueType::Str)]
#[case::timestamp(ValueType::Timestamp)]
fn test_execute_join_accepts_matching_key_types(#[case] value_type: ValueType) {
    let value = value_for_type(value_type);
    assert_eq!(
        execute(
            &parse("from outer join probe on key select key").expect("parses"),
            &operator_scope(),
            None,
            &TypedJoinSource::new(value_type, value_type),
        ),
        Ok(Page {
            outputs: vec![output("key", FieldClass::Public, value_type)],
            rows: vec![vec![value]],
            next_cursor: None,
        })
    );
}

#[rstest]
#[case::bool_int(ValueType::Bool, ValueType::Int)]
#[case::bool_str(ValueType::Bool, ValueType::Str)]
#[case::bool_timestamp(ValueType::Bool, ValueType::Timestamp)]
#[case::int_str(ValueType::Int, ValueType::Str)]
#[case::int_timestamp(ValueType::Int, ValueType::Timestamp)]
#[case::str_timestamp(ValueType::Str, ValueType::Timestamp)]
fn test_execute_join_rejects_mismatched_key_types(#[case] outer_type: ValueType, #[case] probe_type: ValueType) {
    assert_eq!(
        execute(
            &parse("from outer join probe on key select key").expect("parses"),
            &operator_scope(),
            None,
            &TypedJoinSource::new(outer_type, probe_type),
        ),
        Err(PqlError::Validation(format!(
            "join key `key` type differs: `outer` is `{}`, `probe` is `{}`",
            outer_type.as_str(),
            probe_type.as_str()
        )))
    );
}

#[test]
fn test_execute_join_unknown_probe_domain_is_not_disclosed() {
    let result = query(
        "from policy.decisions join ghosts on repository, resource",
        &operator_scope(),
        None,
    );
    assert_eq!(result, Err(PqlError::Unauthorized));
}

#[test]
fn test_execute_join_cursor_is_distinct_and_scope_bound() {
    let scope = operator_scope();
    let first = query(
        "from policy.decisions join usage on repository, resource select resource limit 1",
        &scope,
        None,
    )
    .expect("runs");
    let cursor = cursor::encode("policy.decisions\u{1}usage", &scope, 1);
    assert_eq!(first, resource_page(&["resource-a"], Some(cursor.clone())));

    assert_eq!(
        query("from policy.decisions limit 1", &scope, Some(&cursor)),
        Err(PqlError::InvalidCursor)
    );
    assert_eq!(
        query(
            "from policy.decisions join usage on repository, resource select resource limit 1",
            &repository_scope("alpha"),
            Some(&cursor)
        ),
        Err(PqlError::CursorScopeChanged)
    );
    assert_eq!(
        query(
            "from policy.decisions join usage on repository, resource select resource limit 1",
            &scope,
            Some(&cursor),
        ),
        Ok(resource_page(
            &["resource-b"],
            Some(cursor::encode("policy.decisions\u{1}usage", &scope, 2)),
        ))
    );
}

#[test]
fn test_execute_join_aggregates_probe_metric() {
    let page = query(
        "from policy.decisions join usage on repository, resource aggregate sum(hits) as total by repository order by repository asc",
        &operator_scope(),
        None,
    )
    .expect("runs");
    assert_eq!(
        page,
        Page {
            outputs: vec![
                output("repository", FieldClass::Repository, ValueType::Str),
                output("total", FieldClass::Repository, ValueType::Int),
            ],
            rows: vec![
                vec![Value::Str("alpha".to_owned()), Value::Int(150)],
                vec![Value::Str("other".to_owned()), Value::Int(30)],
            ],
            next_cursor: None,
        }
    );
}

#[test]
fn test_execute_join_rejects_unbounded_probe_domain() {
    let refused = query("from policy.decisions join big on repository", &operator_scope(), None);
    assert!(matches!(refused, Err(PqlError::UnboundedJoin(_))));
}

#[test]
fn test_execute_join_rejects_unbounded_outer_without_leading_filter() {
    let refused = query("from big join policy.decisions on repository", &operator_scope(), None);
    assert!(matches!(refused, Err(PqlError::CostExceeded(_))));
}

#[test]
fn test_execute_join_admits_bounded_outer_with_leading_filter() {
    let page = query(
        r#"from big join policy.decisions on repository where repository == "alpha" select name, resource order by resource asc"#,
        &operator_scope(),
        None,
    )
    .expect("runs");
    assert_eq!(
        page,
        Page {
            outputs: vec![
                output("name", FieldClass::Public, ValueType::Str),
                output("resource", FieldClass::Repository, ValueType::Str),
            ],
            rows: vec![
                vec![Value::Str("resource-a".to_owned()), Value::Str("resource-a".to_owned())],
                vec![Value::Str("resource-a".to_owned()), Value::Str("resource-b".to_owned())],
                vec![Value::Str("resource-a".to_owned()), Value::Str("resource-c".to_owned())],
                vec![Value::Str("resource-a".to_owned()), Value::Str("resource-e".to_owned())],
            ],
            next_cursor: None,
        }
    );
}

struct TypedJoinSource {
    outer: DomainSchema,
    probe: DomainSchema,
    outer_value: Value,
    probe_value: Value,
}

impl TypedJoinSource {
    fn new(outer_type: ValueType, probe_type: ValueType) -> Self {
        Self {
            outer: typed_join_schema("outer", outer_type),
            probe: typed_join_schema("probe", probe_type),
            outer_value: value_for_type(outer_type),
            probe_value: value_for_type(probe_type),
        }
    }
}

impl DataSource for TypedJoinSource {
    fn schema(&self, domain: &str) -> Option<&DomainSchema> {
        assert!(matches!(domain, "outer" | "probe"));
        Some(if domain == "outer" { &self.outer } else { &self.probe })
    }

    fn fetch(&self, domain: &str, _scope: &QueryScope, _filter: Option<&FetchFilter>) -> Result<Vec<Row>, PqlError> {
        assert!(matches!(domain, "outer" | "probe"));
        Ok(vec![Row::new().with(
            "key",
            if domain == "outer" {
                self.outer_value.clone()
            } else {
                self.probe_value.clone()
            },
        )])
    }
}

fn typed_join_schema(name: &'static str, value_type: ValueType) -> DomainSchema {
    DomainSchema {
        name,
        columns: vec![Column::new(
            "key",
            value_type,
            FieldClass::Public,
            Indexability::Indexed,
            false,
        )],
        auth: DomainAuth::OperatorOnly,
        natural_order: "key",
        bounded: true,
        pushdown: &[],
    }
}

fn value_for_type(value_type: ValueType) -> Value {
    match value_type {
        ValueType::Bool => Value::Bool(true),
        ValueType::Int => Value::Int(1),
        ValueType::Str => Value::Str("one".to_owned()),
        ValueType::Timestamp => Value::Timestamp(1),
    }
}

fn output(name: &str, class: FieldClass, value_type: ValueType) -> OutputColumn {
    OutputColumn {
        name: name.to_owned(),
        class,
        value_type,
    }
}

fn resource_page(resources: &[&str], next_cursor: Option<String>) -> Page {
    Page {
        outputs: vec![output("resource", FieldClass::Repository, ValueType::Str)],
        rows: resources
            .iter()
            .map(|resource| vec![Value::Str((*resource).to_owned())])
            .collect(),
        next_cursor,
    }
}

fn resource_time_page(rows: &[(&str, i64)], next_cursor: Option<String>) -> Page {
    Page {
        outputs: vec![
            output("resource", FieldClass::Repository, ValueType::Str),
            output("evaluated_at", FieldClass::Repository, ValueType::Timestamp),
        ],
        rows: rows
            .iter()
            .map(|(resource, timestamp)| vec![Value::Str((*resource).to_owned()), Value::Timestamp(*timestamp)])
            .collect(),
        next_cursor,
    }
}
