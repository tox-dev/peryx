use std::collections::BTreeMap;

use crate::ast::OrderKey;
use crate::catalog::FieldClass;
use crate::error::PqlError;
use crate::parse::{bind, parse};
use crate::plan::{DEFAULT_LIMIT, MAX_LIMIT, OutputColumn, Plan, leading_filter, plan};
use crate::source::FetchFilter;
use crate::value::{Value, ValueType};
use rstest::rstest;

use super::support::{big_schema, schema, unpushed_cheap_schema};

#[test]
fn test_plan_defaults_limit_and_resources_all() {
    assert_eq!(
        plan_text("from policy.decisions"),
        Ok(decision_plan(DEFAULT_LIMIT, Vec::new()))
    );
}

#[test]
fn test_plan_selected_columns_carry_class_and_type() {
    assert_eq!(
        plan_text("from policy.decisions select source, reads"),
        Ok(Plan {
            order_by: Vec::new(),
            limit: DEFAULT_LIMIT,
            outputs: vec![
                OutputColumn {
                    name: "source".to_owned(),
                    class: FieldClass::Operator,
                    value_type: ValueType::Str,
                },
                OutputColumn {
                    name: "reads".to_owned(),
                    class: FieldClass::Repository,
                    value_type: ValueType::Int,
                },
            ],
        })
    );
}

#[test]
fn test_plan_limit_bounds() {
    assert_eq!(
        plan_text("from policy.decisions limit 0"),
        Err(PqlError::Validation("limit must be at least 1".to_owned()))
    );
    assert_eq!(
        plan_text(&format!("from policy.decisions limit {}", MAX_LIMIT + 1)),
        Err(PqlError::Validation(format!("limit must be at most {MAX_LIMIT}")))
    );
    assert_eq!(
        plan_text("from policy.decisions limit 50"),
        Ok(decision_plan(50, Vec::new()))
    );
}

#[rstest]
#[case::comparison("from policy.decisions where nope == 1")]
#[case::selection("from policy.decisions select nope")]
#[case::membership("from policy.decisions where nope in (1)")]
#[case::prefix("from policy.decisions where nope starts_with \"x\"")]
fn test_plan_rejects_unknown_column(#[case] text: &str) {
    assert_eq!(
        plan_text(text),
        Err(PqlError::Validation("unknown column `nope`".to_owned()))
    );
}

#[test]
fn test_plan_type_checks_literals() {
    assert_eq!(
        plan_text(r#"from policy.decisions where reads == "x""#),
        Err(PqlError::Validation(
            "the literal does not match the int column `reads`".to_owned()
        ))
    );
    assert_eq!(
        plan_text("from policy.decisions where state in (1, 2)"),
        Err(PqlError::Validation(
            "the literal does not match the string column `state`".to_owned()
        ))
    );
    assert_eq!(
        plan_text(r#"from policy.decisions where state == "blocked""#),
        Ok(decision_plan(DEFAULT_LIMIT, Vec::new()))
    );
}

#[test]
fn test_plan_rejects_unbound_parameter() {
    assert_eq!(
        plan_text("from policy.decisions where state == :missing"),
        Err(PqlError::Validation("a parameter was left unbound".to_owned()))
    );
}

#[rstest]
#[case::comparison("from policy.decisions where evaluated_at >= :value and state == :value")]
#[case::membership("from policy.decisions where evaluated_at in (:value) and state in (:value)")]
#[case::prefix("from policy.decisions where evaluated_at == :value and resource starts_with :value")]
fn test_plan_rejects_a_parameter_used_by_incompatible_columns(#[case] text: &str) {
    let ast = bind(
        parse(text).expect("parses"),
        &BTreeMap::from([("value".to_owned(), Value::Str("2026-06-01T00:00:00Z".to_owned()))]),
    )
    .expect("binds");
    assert_eq!(
        plan(&ast, &schema()),
        Err(PqlError::Validation(
            "parameter `:value` has incompatible timestamp and string column contexts".to_owned()
        ))
    );
}

#[rstest]
#[case::malformed("not-a-time")]
#[case::out_of_range("10000-01-01T00:00:00Z")]
fn test_plan_rejects_an_invalid_bound_timestamp(#[case] value: &str) {
    let ast = bind(
        parse("from policy.decisions where evaluated_at >= :cutoff").expect("parses"),
        &BTreeMap::from([("cutoff".to_owned(), Value::Str(value.to_owned()))]),
    )
    .expect("binds");
    assert_eq!(
        plan(&ast, &schema()),
        Err(PqlError::Validation(
            "parameter `:cutoff` is not an RFC 3339 timestamp".to_owned()
        ))
    );
}

#[rstest]
#[case::boolean("from policy.decisions where blocked == true")]
#[case::timestamp("from policy.decisions where evaluated_at >= @2026-06-01T00:00:00Z")]
fn test_plan_type_checks_literal(#[case] text: &str) {
    assert_eq!(plan_text(text), Ok(decision_plan(DEFAULT_LIMIT, Vec::new())));
}

#[rstest]
#[case::string(
    r#"from policy.decisions where state < "x""#,
    "`<` is not defined for the string column `state`"
)]
#[case::boolean(
    "from policy.decisions where blocked < true",
    "`<` is not defined for the bool column `blocked`"
)]
fn test_plan_rejects_ordering_on_non_ordered_type(#[case] text: &str, #[case] error: &str) {
    assert_eq!(plan_text(text), Err(PqlError::Validation(error.to_owned())));
}

#[test]
fn test_plan_starts_with_needs_string_column() {
    assert_eq!(
        plan_text("from policy.decisions where reads starts_with \"1\""),
        Err(PqlError::Validation(
            "`starts_with` needs a string column, but `reads` is int".to_owned()
        ))
    );
    assert_eq!(
        plan_text(r#"from policy.decisions where resource starts_with "resource""#),
        Ok(decision_plan(DEFAULT_LIMIT, Vec::new()))
    );
}

#[rstest]
#[case::and(r#"from policy.decisions where state == "blocked" and nope == 1"#)]
#[case::or(r#"from policy.decisions where state == "blocked" or nope == 1"#)]
#[case::not("from policy.decisions where not nope == 1")]
fn test_plan_validates_nested_boolean_predicates(#[case] text: &str) {
    assert_eq!(
        plan_text(text),
        Err(PqlError::Validation("unknown column `nope`".to_owned()))
    );
}

#[test]
fn test_plan_order_must_be_selected() {
    assert_eq!(
        plan_text("from policy.decisions select state order by reads"),
        Err(PqlError::Validation(
            "cannot order by `reads`; it is not a selected column".to_owned()
        ))
    );
    let order_by = vec![OrderKey {
        field: "evaluated_at".to_owned(),
        descending: true,
    }];
    assert_eq!(
        plan_text("from policy.decisions order by evaluated_at desc"),
        Ok(decision_plan(DEFAULT_LIMIT, order_by))
    );
}

#[test]
fn test_plan_aggregate_outputs_and_types() {
    assert_eq!(
        plan_text(
            "from policy.decisions aggregate count() as n, min(evaluated_at) as first, sum(reads) as total by state",
        ),
        Ok(Plan {
            order_by: Vec::new(),
            limit: DEFAULT_LIMIT,
            outputs: vec![
                OutputColumn {
                    name: "state".to_owned(),
                    class: FieldClass::Repository,
                    value_type: ValueType::Str,
                },
                OutputColumn {
                    name: "n".to_owned(),
                    class: FieldClass::Public,
                    value_type: ValueType::Int,
                },
                OutputColumn {
                    name: "first".to_owned(),
                    class: FieldClass::Repository,
                    value_type: ValueType::Timestamp,
                },
                OutputColumn {
                    name: "total".to_owned(),
                    class: FieldClass::Repository,
                    value_type: ValueType::Int,
                },
            ],
        })
    );
}

#[rstest]
#[case::non_numeric(
    "from policy.decisions aggregate sum(state) as x by state",
    "`sum` needs a numeric column, but `state` is not numeric"
)]
#[case::count_column(
    "from policy.decisions aggregate count(reads) as x by state",
    "`count` takes no column"
)]
#[case::missing_column("from policy.decisions aggregate sum() as x by state", "`sum` needs a column")]
fn test_plan_rejects_invalid_aggregate(#[case] text: &str, #[case] error: &str) {
    assert_eq!(plan_text(text), Err(PqlError::Validation(error.to_owned())));
}

#[test]
fn test_plan_aggregate_rejects_empty_alias() {
    let ast = crate::ast::Ast {
        domain: "policy.decisions".to_owned(),
        join: None,
        predicate: None,
        selection: crate::ast::Selection::All,
        aggregate: Some(crate::ast::Aggregate {
            terms: vec![crate::ast::AggregateTerm {
                func: crate::ast::AggregateFunc::Count,
                column: None,
                alias: String::new(),
            }],
            group_by: vec!["state".to_owned()],
        }),
        order_by: Vec::new(),
        limit: None,
    };
    assert_eq!(
        plan(&ast, &schema()),
        Err(PqlError::Validation("an aggregate needs an alias".to_owned()))
    );
}

#[test]
fn test_plan_unbounded_group_key_must_be_cheap() {
    let refused = plan(
        &parse("from big where repository == \"alpha\" aggregate count() as n by name").expect("parses"),
        &big_schema(),
    );
    assert_eq!(
        refused,
        Err(PqlError::Validation(
            "group key `name` is not cheap to group on".to_owned()
        ))
    );
    assert_eq!(
        plan(
            &parse("from big where repository == \"alpha\" aggregate count() as n by repository").expect("parses"),
            &big_schema()
        ),
        Ok(Plan {
            order_by: Vec::new(),
            limit: DEFAULT_LIMIT,
            outputs: vec![
                OutputColumn {
                    name: "repository".to_owned(),
                    class: FieldClass::Repository,
                    value_type: ValueType::Str,
                },
                OutputColumn {
                    name: "n".to_owned(),
                    class: FieldClass::Public,
                    value_type: ValueType::Int,
                },
            ],
        })
    );
}

#[test]
fn test_cost_gate_bounded_domain_always_admits() {
    assert_eq!(
        plan_text("from policy.decisions"),
        Ok(decision_plan(DEFAULT_LIMIT, Vec::new()))
    );
}

#[rstest]
#[case::equality(
    "from big where repository == \"alpha\"",
    FetchFilter {
        column: "repository",
        values: vec![Value::Str("alpha".to_owned())],
    }
)]
#[case::membership(
    "from big where repository in (\"alpha\")",
    FetchFilter {
        column: "repository",
        values: vec![Value::Str("alpha".to_owned())],
    }
)]
#[case::indexed_conjunction(
    "from big where name starts_with \"n\" and repository == \"alpha\"",
    FetchFilter {
        column: "repository",
        values: vec![Value::Str("alpha".to_owned())],
    }
)]
fn test_cost_gate_unbounded_admits_pushdown_filter(#[case] text: &str, #[case] expected_filter: FetchFilter) {
    let big = big_schema();
    let ast = parse(text).expect("parses");
    assert_eq!(
        leading_filter(ast.predicate.as_ref().expect("has predicate"), &big),
        Some(expected_filter)
    );
    assert_eq!(plan(&ast, &big), Ok(big_plan()));
}

#[rstest]
#[case::scan("from big where name == \"resource-a\"")]
#[case::missing("from big")]
fn test_cost_gate_unbounded_refuses_without_pushdown_filter(#[case] text: &str) {
    assert!(matches!(
        plan(&parse(text).expect("parses"), &big_schema()),
        Err(PqlError::CostExceeded(_))
    ));
}

#[test]
fn test_cost_gate_refuses_a_cheap_column_the_source_does_not_push_down() {
    let schema = unpushed_cheap_schema();

    let refuse = plan(&parse("from big where shard == \"a\"").expect("parses"), &schema);
    assert_eq!(
        refuse,
        Err(PqlError::CostExceeded(
            "`big` is large; add an equality filter on an indexed column".to_owned()
        ))
    );

    let ast = parse("from big where repository == \"a\"").expect("parses");
    assert_eq!(
        leading_filter(ast.predicate.as_ref().expect("has predicate"), &schema),
        Some(FetchFilter {
            column: "repository",
            values: vec![Value::Str("a".to_owned())],
        })
    );
    assert_eq!(plan(&ast, &schema), Ok(unpushed_plan()));
}

#[test]
fn test_leading_filter_skips_a_cheap_but_unpushed_column() {
    let ast = parse("from big where shard == \"a\"").expect("parses");
    let predicate = ast.predicate.as_ref().expect("has a predicate");

    assert_eq!(leading_filter(predicate, &unpushed_cheap_schema()), None);
}

#[rstest]
#[case::equality(
    r#"from policy.decisions where repository == "alpha""#,
    FetchFilter {
        column: "repository",
        values: vec![Value::Str("alpha".to_owned())],
    }
)]
#[case::membership(
    r#"from policy.decisions where resource in ("resource-a", "resource-b")"#,
    FetchFilter {
        column: "resource",
        values: vec![Value::Str("resource-a".to_owned()), Value::Str("resource-b".to_owned())],
    }
)]
#[case::timestamp(
    "from policy.decisions where evaluated_at == @2026-06-01T00:00:00Z",
    FetchFilter {
        column: "evaluated_at",
        values: vec![Value::Timestamp(1_780_272_000)],
    }
)]
fn test_leading_filter_extracts_pushdown(#[case] text: &str, #[case] expected: FetchFilter) {
    assert_eq!(filter_of(text), Some(expected));
}

#[rstest]
#[case::left(r#"from policy.decisions where resource == "resource-a" and state == "blocked""#)]
#[case::right(r#"from policy.decisions where state == "blocked" and resource == "resource-a""#)]
fn test_leading_filter_picks_pushdown_side_of_and(#[case] text: &str) {
    assert_eq!(
        filter_of(text),
        Some(FetchFilter {
            column: "resource",
            values: vec![Value::Str("resource-a".to_owned())],
        })
    );
}

#[rstest]
#[case::scan(r#"from policy.decisions where state == "blocked""#)]
#[case::disjunction(r#"from policy.decisions where repository == "alpha" or resource == "resource-a""#)]
#[case::unbound("from policy.decisions where repository == :repo")]
fn test_leading_filter_absent(#[case] text: &str) {
    assert_eq!(filter_of(text), None);
}

#[rstest]
#[case::disjunction("from big where repository == \"alpha\" or name == \"x\"")]
#[case::negation("from big where not repository == \"alpha\"")]
fn test_cost_gate_ignores_non_conjunctive_filter(#[case] text: &str) {
    assert_eq!(
        plan(&parse(text).expect("parses"), &big_schema()),
        Err(PqlError::CostExceeded(
            "`big` is large; add an equality filter on an indexed column".to_owned()
        ))
    );
}

fn filter_of(text: &str) -> Option<FetchFilter> {
    let ast = parse(text).expect("parses");
    leading_filter(ast.predicate.as_ref().expect("has a predicate"), &schema())
}

fn plan_text(text: &str) -> Result<Plan, PqlError> {
    plan(&parse(text).expect("parses"), &schema())
}

fn decision_plan(limit: u32, order_by: Vec<OrderKey>) -> Plan {
    Plan {
        order_by,
        limit,
        outputs: vec![
            OutputColumn {
                name: "repository".to_owned(),
                class: FieldClass::Repository,
                value_type: ValueType::Str,
            },
            OutputColumn {
                name: "resource".to_owned(),
                class: FieldClass::Repository,
                value_type: ValueType::Str,
            },
            OutputColumn {
                name: "state".to_owned(),
                class: FieldClass::Repository,
                value_type: ValueType::Str,
            },
            OutputColumn {
                name: "source".to_owned(),
                class: FieldClass::Operator,
                value_type: ValueType::Str,
            },
            OutputColumn {
                name: "reads".to_owned(),
                class: FieldClass::Repository,
                value_type: ValueType::Int,
            },
            OutputColumn {
                name: "blocked".to_owned(),
                class: FieldClass::Repository,
                value_type: ValueType::Bool,
            },
            OutputColumn {
                name: "evaluated_at".to_owned(),
                class: FieldClass::Repository,
                value_type: ValueType::Timestamp,
            },
        ],
    }
}

fn big_plan() -> Plan {
    Plan {
        order_by: Vec::new(),
        limit: DEFAULT_LIMIT,
        outputs: vec![
            OutputColumn {
                name: "repository".to_owned(),
                class: FieldClass::Repository,
                value_type: ValueType::Str,
            },
            OutputColumn {
                name: "name".to_owned(),
                class: FieldClass::Public,
                value_type: ValueType::Str,
            },
        ],
    }
}

fn unpushed_plan() -> Plan {
    Plan {
        order_by: Vec::new(),
        limit: DEFAULT_LIMIT,
        outputs: vec![
            OutputColumn {
                name: "repository".to_owned(),
                class: FieldClass::Repository,
                value_type: ValueType::Str,
            },
            OutputColumn {
                name: "shard".to_owned(),
                class: FieldClass::Repository,
                value_type: ValueType::Str,
            },
        ],
    }
}
