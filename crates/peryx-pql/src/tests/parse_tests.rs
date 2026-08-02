use std::collections::BTreeMap;

use crate::ast::{AggregateFunc, CompareOp, Literal, Predicate, Selection};
use crate::error::PqlError;
use crate::parse::{MAX_QUERY_BYTES, Params, bind, parse};
use crate::value::Value;

fn params(pairs: &[(&str, Value)]) -> Params {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

#[test]
fn test_parse_minimal_query() {
    let ast = parse("from policy.decisions").expect("parses");
    assert_eq!(ast.domain, "policy.decisions");
    assert!(ast.predicate.is_none());
    assert_eq!(ast.selection, Selection::All);
    assert!(ast.join.is_none());
    assert!(ast.order_by.is_empty());
    assert_eq!(ast.limit, None);
}

#[test]
fn test_parse_full_query_shape() {
    let ast = parse(
        r#"from policy.decisions where state == "blocked" and downloads >= 5 select repository, state order by evaluated_at desc, project asc limit 10"#,
    )
    .expect("parses");
    assert_eq!(
        ast.selection,
        Selection::Columns(vec!["repository".to_owned(), "state".to_owned()])
    );
    assert_eq!(ast.order_by.len(), 2);
    assert!(ast.order_by[0].descending);
    assert!(!ast.order_by[1].descending);
    assert_eq!(ast.limit, Some(10));
    assert!(matches!(ast.predicate, Some(Predicate::And(_, _))));
}

#[test]
fn test_parse_select_star_is_all() {
    let ast = parse("from d select *").expect("parses");
    assert_eq!(ast.selection, Selection::All);
}

#[test]
fn test_parse_predicate_operators_and_literals() {
    let ast =
        parse(r#"from d where a == "s" or b != 3 or c < 1 or d <= 2 or e > 3 or f >= 4 or g == true or h == false"#)
            .expect("parses");
    assert!(matches!(ast.predicate, Some(Predicate::Or(_, _))));
}

#[test]
fn test_parse_in_starts_with_not_and_parens() {
    let ast = parse(r#"from d where not (state in ("a", "b") and project starts_with "num")"#).expect("parses");
    assert!(matches!(ast.predicate, Some(Predicate::Not(_))));
}

#[test]
fn test_parse_timestamp_literal() {
    let ast = parse("from d where evaluated_at >= @2026-06-01T00:00:00Z").expect("parses");
    let Some(Predicate::Compare { op, value, .. }) = ast.predicate else {
        panic!("expected a comparison");
    };
    assert_eq!(op, CompareOp::Ge);
    assert!(matches!(value, Literal::Timestamp(_)));
}

#[test]
fn test_parse_negative_integer() {
    let ast = parse("from d where n == -5").expect("parses");
    let Some(Predicate::Compare { value, .. }) = ast.predicate else {
        panic!("expected a comparison");
    };
    assert_eq!(value, Literal::Int(-5));
}

#[test]
fn test_parse_string_escapes() {
    let ast = parse(r#"from d where a == "he said \"hi\" \\ ok""#).expect("parses");
    let Some(Predicate::Compare { value, .. }) = ast.predicate else {
        panic!("expected a comparison");
    };
    assert_eq!(value, Literal::Str(r#"he said "hi" \ ok"#.to_owned()));
}

#[test]
fn test_parse_non_ascii_string_literal() {
    // The byte range between the quotes is decoded as UTF-8, so a multibyte char is one codepoint, not
    // a run of Latin-1 mojibake.
    let ast = parse(r#"from d where name == "café — naïve""#).expect("parses");
    let Some(Predicate::Compare { value, .. }) = ast.predicate else {
        panic!("expected a comparison");
    };
    assert_eq!(value, Literal::Str("café — naïve".to_owned()));
}

#[test]
fn test_parse_non_ascii_string_with_escape() {
    let ast = parse(r#"from d where name == "café \"x\"""#).expect("parses");
    let Some(Predicate::Compare { value, .. }) = ast.predicate else {
        panic!("expected a comparison");
    };
    assert_eq!(value, Literal::Str(r#"café "x""#.to_owned()));
}

#[test]
fn test_parse_aggregate() {
    let ast = parse("from d aggregate count() as n, sum(downloads) as total by state, project").expect("parses");
    let aggregate = ast.aggregate.expect("has aggregate");
    assert_eq!(aggregate.group_by, vec!["state".to_owned(), "project".to_owned()]);
    assert_eq!(aggregate.terms.len(), 2);
    assert_eq!(aggregate.terms[0].func, AggregateFunc::Count);
    assert_eq!(aggregate.terms[0].column, None);
    assert_eq!(aggregate.terms[1].func, AggregateFunc::Sum);
    assert_eq!(aggregate.terms[1].column, Some("downloads".to_owned()));
}

#[test]
fn test_parse_count_star() {
    let ast = parse("from d aggregate count(*) as n by state").expect("parses");
    assert_eq!(ast.aggregate.expect("aggregate").terms[0].column, None);
}

#[test]
fn test_parse_join_grammar() {
    let ast = parse("from trash join retention on project where restorable == true").expect("parses");
    let join = ast.join.expect("has join");
    assert_eq!(join.domain, "retention");
    assert_eq!(join.on, "project");
}

#[test]
fn test_bind_replaces_each_param_type() {
    let ast = parse(r"from d where a == :s and b == :i and c == :b and t == :ts").expect("parses");
    let bound = bind(
        ast,
        &params(&[
            ("s", Value::Str("x".to_owned())),
            ("i", Value::Int(3)),
            ("b", Value::Bool(true)),
            ("ts", Value::Timestamp(100)),
        ]),
    )
    .expect("binds");
    let flattened = format!("{:?}", bound.predicate);
    assert!(flattened.contains("Str(\"x\")"));
    assert!(flattened.contains("Int(3)"));
    assert!(flattened.contains("Bool(true)"));
    assert!(flattened.contains("Timestamp(100)"));
}

#[test]
fn test_bind_missing_parameter_is_rejected() {
    let ast = parse("from d where a == :missing").expect("parses");
    assert_eq!(
        bind(ast, &BTreeMap::new()),
        Err(PqlError::MissingParameter("missing".to_owned()))
    );
}

#[test]
fn test_bind_null_parameter_is_missing() {
    let ast = parse("from d where a == :n").expect("parses");
    assert_eq!(
        bind(ast, &params(&[("n", Value::Null)])),
        Err(PqlError::MissingParameter("n".to_owned()))
    );
}

#[test]
fn test_bind_in_and_starts_with_params() {
    let ast = parse(r#"from d where a in (:x, "b") and c starts_with :p"#).expect("parses");
    let bound = bind(
        ast,
        &params(&[("x", Value::Str("a".to_owned())), ("p", Value::Str("n".to_owned()))]),
    )
    .expect("binds");
    assert!(bound.predicate.is_some());
}

#[test]
fn test_bind_without_predicate_is_noop() {
    let ast = parse("from d").expect("parses");
    assert!(bind(ast, &BTreeMap::new()).expect("binds").predicate.is_none());
}

#[test]
fn test_parse_rejects_oversized_text() {
    let text = format!("from d where a == \"{}\"", "x".repeat(MAX_QUERY_BYTES));
    assert!(matches!(parse(&text), Err(PqlError::Parse(_))));
}

#[test]
fn test_parse_rejects_deep_nesting() {
    let text = format!("from d where {}a == 1{}", "(".repeat(40), ")".repeat(40));
    assert!(matches!(parse(&text), Err(PqlError::Parse(_))));
}

#[test]
fn test_parse_rejects_deep_boolean_chains() {
    // A long conjunction, disjunction, or `not` run builds a tree the evaluator would recurse down,
    // so the depth cap covers boolean nesting, not just parentheses.
    let conjunction = format!("from d where {}", vec!["a == 1"; 60].join(" and "));
    assert!(matches!(parse(&conjunction), Err(PqlError::Parse(_))));
    let disjunction = format!("from d where {}", vec!["a == 1"; 60].join(" or "));
    assert!(matches!(parse(&disjunction), Err(PqlError::Parse(_))));
    let negations = format!("from d where {}a == 1", "not ".repeat(60));
    assert!(matches!(parse(&negations), Err(PqlError::Parse(_))));
    // A modest boolean query stays well under the cap.
    assert!(parse("from d where a == 1 and b == 2 and c == 3 or not d == 4").is_ok());
}

#[test]
fn test_parse_error_cases() {
    for text in [
        "",
        "select *",
        "from",
        "from d where",
        "from d where a =! 1",
        "from d where a == \"open",
        "from d where a == @notatime",
        "from d where a == :",
        "from d where a == 999999999999999999999999",
        "from d where a == )",
        "from d limit x",
        "from d limit -1",
        "from d aggregate bogus(x) as y by z",
        "from d aggregate count) as y by z",
        "from d extra",
        "from d where a in 1",
        "from d where # broken",
    ] {
        assert!(
            matches!(parse(text), Err(PqlError::Parse(_))),
            "expected parse error for `{text}`"
        );
    }
}

#[test]
fn test_parse_limit_accepts_zero_token() {
    // The parser accepts a syntactic zero; the planner is what rejects it.
    assert_eq!(parse("from d limit 0").expect("parses").limit, Some(0));
}
