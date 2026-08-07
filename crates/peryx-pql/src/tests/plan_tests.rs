use crate::catalog::FieldClass;
use crate::error::PqlError;
use crate::parse::parse;
use crate::plan::{DEFAULT_LIMIT, MAX_LIMIT, leading_filter, plan};
use crate::source::FetchFilter;
use crate::value::{Value, ValueType};

use super::support::{big_schema, schema, unpushed_cheap_schema};

fn filter_of(text: &str) -> Option<FetchFilter> {
    let ast = parse(text).expect("parses");
    leading_filter(ast.predicate.as_ref().expect("has a predicate"), &schema())
}

fn plan_text(text: &str) -> Result<crate::plan::Plan, PqlError> {
    plan(&parse(text).expect("parses"), &schema())
}

#[test]
fn test_plan_defaults_limit_and_projects_all() {
    let plan = plan_text("from policy.decisions").expect("plans");
    assert_eq!(plan.limit, DEFAULT_LIMIT);
    assert_eq!(plan.outputs.len(), schema().columns.len());
}

#[test]
fn test_plan_selected_columns_carry_class_and_type() {
    let plan = plan_text("from policy.decisions select source, downloads").expect("plans");
    assert_eq!(plan.outputs.len(), 2);
    assert_eq!(plan.outputs[0].name, "source");
    assert_eq!(plan.outputs[0].class, FieldClass::Operator);
    assert_eq!(plan.outputs[1].value_type, ValueType::Int);
}

#[test]
fn test_plan_limit_bounds() {
    assert!(matches!(
        plan_text("from policy.decisions limit 0"),
        Err(PqlError::Validation(_))
    ));
    assert!(matches!(
        plan_text(&format!("from policy.decisions limit {}", MAX_LIMIT + 1)),
        Err(PqlError::Validation(_))
    ));
    assert_eq!(plan_text("from policy.decisions limit 50").expect("plans").limit, 50);
}

#[test]
fn test_plan_rejects_unknown_column_everywhere() {
    for text in [
        "from policy.decisions where nope == 1",
        "from policy.decisions select nope",
        "from policy.decisions where nope in (1)",
        "from policy.decisions where nope starts_with \"x\"",
    ] {
        assert!(matches!(plan_text(text), Err(PqlError::Validation(_))), "for `{text}`");
    }
}

#[test]
fn test_plan_type_checks_literals() {
    assert!(matches!(
        plan_text(r#"from policy.decisions where downloads == "x""#),
        Err(PqlError::Validation(_))
    ));
    assert!(matches!(
        plan_text("from policy.decisions where state in (1, 2)"),
        Err(PqlError::Validation(_))
    ));
    assert!(plan_text(r#"from policy.decisions where state == "blocked""#).is_ok());
}

#[test]
fn test_plan_rejects_unbound_parameter() {
    // A literal left as a parameter placeholder means binding never ran; validation refuses it rather
    // than treating the hole as a value.
    assert!(matches!(
        plan_text("from policy.decisions where state == :missing"),
        Err(PqlError::Validation(_))
    ));
}

#[test]
fn test_plan_type_checks_bool_and_timestamp_literals() {
    // Equality on a bool column and a range comparison on a timestamp column each type-check their
    // literal against the column, so the bool and timestamp literal arms both run and admit the query.
    let with_bool = plan_text("from policy.decisions where blocked == true").expect("plans");
    assert_eq!(with_bool.outputs.len(), schema().columns.len());
    let with_timestamp = plan_text("from policy.decisions where evaluated_at >= @2026-06-01T00:00:00Z").expect("plans");
    assert_eq!(with_timestamp.outputs.len(), schema().columns.len());
}

#[test]
fn test_plan_rejects_ordering_on_text_and_bool() {
    assert!(matches!(
        plan_text(r#"from policy.decisions where state < "x""#),
        Err(PqlError::Validation(_))
    ));
    assert!(matches!(
        plan_text("from policy.decisions where blocked < true"),
        Err(PqlError::Validation(_))
    ));
}

#[test]
fn test_plan_starts_with_needs_string_column() {
    assert!(matches!(
        plan_text("from policy.decisions where downloads starts_with \"1\""),
        Err(PqlError::Validation(_))
    ));
    assert!(plan_text(r#"from policy.decisions where project starts_with "num""#).is_ok());
}

#[test]
fn test_plan_boolean_and_not_recurse() {
    assert!(plan_text(r#"from policy.decisions where not (state == "blocked" or downloads >= 1)"#).is_ok());
}

#[test]
fn test_plan_order_must_be_selected() {
    assert!(matches!(
        plan_text("from policy.decisions select state order by downloads"),
        Err(PqlError::Validation(_))
    ));
    assert!(plan_text("from policy.decisions order by evaluated_at desc").is_ok());
}

#[test]
fn test_plan_aggregate_outputs_and_types() {
    let plan = plan_text(
        "from policy.decisions aggregate count() as n, min(evaluated_at) as first, sum(downloads) as total by state",
    )
    .expect("plans");
    assert_eq!(plan.outputs[0].name, "state");
    assert_eq!(plan.outputs[1].name, "n");
    assert_eq!(plan.outputs[1].class, FieldClass::Public);
    assert_eq!(plan.outputs[1].value_type, ValueType::Int);
    assert_eq!(plan.outputs[2].name, "first");
    assert_eq!(plan.outputs[2].value_type, ValueType::Timestamp);
    assert_eq!(plan.outputs[3].value_type, ValueType::Int);
}

#[test]
fn test_plan_aggregate_rejections() {
    for text in [
        "from policy.decisions aggregate sum(state) as x by state",
        "from policy.decisions aggregate count(downloads) as x by state",
        "from policy.decisions aggregate sum() as x by state",
    ] {
        assert!(matches!(plan_text(text), Err(PqlError::Validation(_))), "for `{text}`");
    }
}

#[test]
fn test_plan_aggregate_rejects_empty_alias() {
    // The parser always reads a non-empty alias after `as`, so the empty-alias guard is reachable only
    // from a hand-built tree; drive it directly through the public AST.
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
    // `name` is a scan column on the unbounded `big` domain, so grouping on it is refused before the
    // cost gate even sees the query.
    let refused = plan(
        &parse("from big where repository == \"alpha\" aggregate count() as n by name").expect("parses"),
        &big_schema(),
    );
    assert!(matches!(refused, Err(PqlError::Validation(_))));
    assert!(
        plan(
            &parse("from big where repository == \"alpha\" aggregate count() as n by repository").expect("parses"),
            &big_schema()
        )
        .is_ok()
    );
}

#[test]
fn test_cost_gate_bounded_domain_always_admits() {
    assert!(plan_text("from policy.decisions").is_ok());
}

#[test]
fn test_cost_gate_unbounded_requires_cheap_leading_filter() {
    let big = big_schema();
    let refuse = plan(&parse("from big where name == \"numpy\"").expect("parses"), &big);
    assert!(matches!(refuse, Err(PqlError::CostExceeded(_))));

    let no_filter = plan(&parse("from big").expect("parses"), &big);
    assert!(matches!(no_filter, Err(PqlError::CostExceeded(_))));

    assert!(plan(&parse("from big where repository == \"alpha\"").expect("parses"), &big).is_ok());
    assert!(
        plan(
            &parse("from big where repository in (\"alpha\")").expect("parses"),
            &big
        )
        .is_ok()
    );
    assert!(
        plan(
            &parse("from big where name starts_with \"n\" and repository == \"alpha\"").expect("parses"),
            &big
        )
        .is_ok()
    );
}

#[test]
fn test_cost_gate_refuses_a_cheap_column_the_source_does_not_push_down() {
    let schema = unpushed_cheap_schema();

    // `shard` is `Indexed`, so a gate keyed on `is_cheap` would admit an unbounded scan led by it, but
    // the source does not narrow its fetch on `shard`, so the aligned gate refuses.
    let refuse = plan(&parse("from big where shard == \"a\"").expect("parses"), &schema);
    assert!(matches!(refuse, Err(PqlError::CostExceeded(_))));

    // The pushdown column still admits.
    assert!(plan(&parse("from big where repository == \"a\"").expect("parses"), &schema).is_ok());
}

#[test]
fn test_leading_filter_skips_a_cheap_but_unpushed_column() {
    let ast = parse("from big where shard == \"a\"").expect("parses");
    let predicate = ast.predicate.as_ref().expect("has a predicate");

    assert_eq!(leading_filter(predicate, &unpushed_cheap_schema()), None);
}

#[test]
fn test_fetch_filter_debug_clone_and_eq() {
    // The filter crosses the DataSource seam and is compared and logged there, so its derived Debug,
    // Clone and Eq are load-bearing and must each run under coverage before an assertion fails.
    let filter = FetchFilter {
        column: "repository",
        values: vec![Value::Str("alpha".to_owned())],
    };
    assert_eq!(filter.clone(), filter);
    let rendered = format!("{filter:?}");
    assert!(rendered.contains("repository"));
    assert!(rendered.contains("alpha"));
}

#[test]
fn test_leading_filter_extracts_indexed_equality() {
    assert_eq!(
        filter_of(r#"from policy.decisions where repository == "alpha""#),
        Some(FetchFilter {
            column: "repository",
            values: vec![Value::Str("alpha".to_owned())],
        })
    );
    assert_eq!(
        filter_of(r#"from policy.decisions where project in ("numpy", "scipy")"#),
        Some(FetchFilter {
            column: "project",
            values: vec![Value::Str("numpy".to_owned()), Value::Str("scipy".to_owned())],
        })
    );
    assert_eq!(
        filter_of("from policy.decisions where evaluated_at == @2026-06-01T00:00:00Z")
            .expect("timestamp key is cheap")
            .column,
        "evaluated_at"
    );
}

#[test]
fn test_leading_filter_picks_the_cheap_side_of_an_and() {
    let left_cheap = filter_of(r#"from policy.decisions where project == "numpy" and state == "blocked""#);
    let right_cheap = filter_of(r#"from policy.decisions where state == "blocked" and project == "numpy""#);
    assert_eq!(left_cheap.expect("left is indexed").column, "project");
    assert_eq!(right_cheap.expect("right is indexed").column, "project");
}

#[test]
fn test_leading_filter_absent_for_scan_or_or_or_unbound() {
    // A scan column, a disjunction, and an unbound parameter each yield no indexed narrowing.
    assert_eq!(filter_of(r#"from policy.decisions where state == "blocked""#), None);
    assert_eq!(
        filter_of(r#"from policy.decisions where repository == "alpha" or project == "numpy""#),
        None
    );
    assert_eq!(filter_of("from policy.decisions where repository == :repo"), None);
}

#[test]
fn test_cost_gate_ignores_or_and_not_as_leading() {
    let big = big_schema();
    let disjunction = plan(
        &parse("from big where repository == \"alpha\" or name == \"x\"").expect("parses"),
        &big,
    );
    assert!(matches!(disjunction, Err(PqlError::CostExceeded(_))));
}
