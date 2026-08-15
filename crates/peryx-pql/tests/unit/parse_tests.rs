use std::collections::BTreeMap;

use crate::ast::{
    Aggregate, AggregateFunc, AggregateTerm, Ast, CompareOp, Join, Literal, OrderKey, Predicate, Selection,
};
use crate::error::PqlError;
use crate::parse::{MAX_QUERY_BYTES, Params, bind, parse};
use crate::value::Value;
use rstest::rstest;

#[test]
fn test_parse_minimal_query() {
    assert_eq!(parse("from policy.decisions"), Ok(ast("policy.decisions")));
}

#[test]
fn test_parse_full_query_shape() {
    assert_eq!(
        parse(
            r#"from policy.decisions where state == "blocked" and reads >= 5 select repository, state order by evaluated_at desc, resource asc limit 10"#,
        ),
        Ok(Ast {
            domain: "policy.decisions".to_owned(),
            join: None,
            predicate: Some(Predicate::And(
                Box::new(compare("state", CompareOp::Eq, Literal::Str("blocked".to_owned()))),
                Box::new(compare("reads", CompareOp::Ge, Literal::Int(5))),
            )),
            selection: Selection::Columns(vec!["repository".to_owned(), "state".to_owned()]),
            aggregate: None,
            order_by: vec![
                OrderKey {
                    field: "evaluated_at".to_owned(),
                    descending: true,
                },
                OrderKey {
                    field: "resource".to_owned(),
                    descending: false,
                },
            ],
            limit: Some(10),
        })
    );
}

#[test]
fn test_parse_select_star_is_all() {
    assert_eq!(parse("from d select *"), Ok(ast("d")));
}

#[rstest]
#[case::string(r#"from d where field == "s""#, CompareOp::Eq, Literal::Str("s".to_owned()))]
#[case::not_equal("from d where field != 3", CompareOp::Ne, Literal::Int(3))]
#[case::less_than("from d where field < 1", CompareOp::Lt, Literal::Int(1))]
#[case::less_or_equal("from d where field <= 2", CompareOp::Le, Literal::Int(2))]
#[case::greater_than("from d where field > 3", CompareOp::Gt, Literal::Int(3))]
#[case::greater_or_equal("from d where field >= 4", CompareOp::Ge, Literal::Int(4))]
#[case::true_literal("from d where field == true", CompareOp::Eq, Literal::Bool(true))]
#[case::false_literal("from d where field == false", CompareOp::Eq, Literal::Bool(false))]
fn test_parse_comparison(#[case] text: &str, #[case] op: CompareOp, #[case] value: Literal) {
    assert_eq!(
        parse(text),
        Ok(Ast {
            predicate: Some(compare("field", op, value)),
            ..ast("d")
        })
    );
}

#[test]
fn test_parse_in_starts_with_not_and_parens() {
    assert_eq!(
        parse(r#"from d where not (state in ("a", "b") and resource starts_with "resource")"#),
        Ok(Ast {
            predicate: Some(Predicate::Not(Box::new(Predicate::And(
                Box::new(Predicate::In {
                    field: "state".to_owned(),
                    values: vec![Literal::Str("a".to_owned()), Literal::Str("b".to_owned())],
                }),
                Box::new(Predicate::StartsWith {
                    field: "resource".to_owned(),
                    prefix: Literal::Str("resource".to_owned()),
                }),
            )))),
            ..ast("d")
        })
    );
}

#[test]
fn test_parse_timestamp_literal() {
    assert_eq!(
        parse("from d where evaluated_at >= @2026-06-01T00:00:00Z"),
        Ok(Ast {
            predicate: Some(compare(
                "evaluated_at",
                CompareOp::Ge,
                Literal::Timestamp(1_780_272_000),
            )),
            ..ast("d")
        })
    );
}

#[test]
fn test_parse_negative_integer() {
    assert_eq!(
        parse("from d where n == -5"),
        Ok(Ast {
            predicate: Some(compare("n", CompareOp::Eq, Literal::Int(-5))),
            ..ast("d")
        })
    );
}

#[rstest]
#[case::escaped(r#"from d where name == "he said \"hi\" \\ ok""#, r#"he said "hi" \ ok"#)]
#[case::non_ascii(r#"from d where name == "café - naïve""#, "café - naïve")]
#[case::non_ascii_escaped(r#"from d where name == "café \"x\"""#, r#"café "x""#)]
fn test_parse_string(#[case] text: &str, #[case] expected: &str) {
    assert_eq!(
        parse(text),
        Ok(Ast {
            predicate: Some(compare("name", CompareOp::Eq, Literal::Str(expected.to_owned()))),
            ..ast("d")
        })
    );
}

#[test]
fn test_parse_aggregate() {
    assert_eq!(
        parse("from d aggregate count() as n, sum(reads) as total by state, resource"),
        Ok(Ast {
            aggregate: Some(Aggregate {
                terms: vec![
                    AggregateTerm {
                        func: AggregateFunc::Count,
                        column: None,
                        alias: "n".to_owned(),
                    },
                    AggregateTerm {
                        func: AggregateFunc::Sum,
                        column: Some("reads".to_owned()),
                        alias: "total".to_owned(),
                    },
                ],
                group_by: vec!["state".to_owned(), "resource".to_owned()],
            }),
            ..ast("d")
        })
    );
}

#[test]
fn test_parse_count_star() {
    assert_eq!(
        parse("from d aggregate count(*) as n by state"),
        Ok(Ast {
            aggregate: Some(Aggregate {
                terms: vec![AggregateTerm {
                    func: AggregateFunc::Count,
                    column: None,
                    alias: "n".to_owned(),
                }],
                group_by: vec!["state".to_owned()],
            }),
            ..ast("d")
        })
    );
}

#[test]
fn test_parse_join_grammar() {
    assert_eq!(
        parse("from trash join retention on resource where restorable == true"),
        Ok(Ast {
            domain: "trash".to_owned(),
            join: Some(Join {
                domain: "retention".to_owned(),
                on: vec!["resource".to_owned()],
            }),
            predicate: Some(compare("restorable", CompareOp::Eq, Literal::Bool(true))),
            ..ast("trash")
        })
    );
}

#[test]
fn test_parse_join_composite_key() {
    assert_eq!(
        parse("from policy.decisions join usage on repository, resource"),
        Ok(Ast {
            join: Some(Join {
                domain: "usage".to_owned(),
                on: vec!["repository".to_owned(), "resource".to_owned()],
            }),
            ..ast("policy.decisions")
        })
    );
}

#[test]
fn test_bind_replaces_each_param_type() {
    let parsed = parse(r"from d where a == :s and b == :i and c == :b and t == :ts").expect("parses");
    let bound = bind(
        parsed,
        &params(&[
            ("s", Value::Str("x".to_owned())),
            ("i", Value::Int(3)),
            ("b", Value::Bool(true)),
            ("ts", Value::Timestamp(100)),
        ]),
    )
    .expect("binds");
    assert_eq!(
        bound,
        Ast {
            predicate: Some(Predicate::And(
                Box::new(Predicate::And(
                    Box::new(Predicate::And(
                        Box::new(compare("a", CompareOp::Eq, Literal::Str("x".to_owned()))),
                        Box::new(compare("b", CompareOp::Eq, Literal::Int(3))),
                    )),
                    Box::new(compare("c", CompareOp::Eq, Literal::Bool(true))),
                )),
                Box::new(compare("t", CompareOp::Eq, Literal::Timestamp(100))),
            )),
            ..ast("d")
        }
    );
}

#[test]
fn test_bind_replaces_params_under_or_and_not() {
    let parsed = parse("from d where a == :x or not b == :y").expect("parses");
    let bound = bind(
        parsed,
        &params(&[("x", Value::Str("left".to_owned())), ("y", Value::Int(7))]),
    )
    .expect("binds");
    assert_eq!(
        bound,
        Ast {
            predicate: Some(Predicate::Or(
                Box::new(compare("a", CompareOp::Eq, Literal::Str("left".to_owned()))),
                Box::new(Predicate::Not(Box::new(compare("b", CompareOp::Eq, Literal::Int(7))))),
            )),
            ..ast("d")
        }
    );
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
    let parsed = parse(r#"from d where a in (:x, "b") and c starts_with :p"#).expect("parses");
    let bound = bind(
        parsed,
        &params(&[("x", Value::Str("a".to_owned())), ("p", Value::Str("n".to_owned()))]),
    )
    .expect("binds");
    assert_eq!(
        bound,
        Ast {
            predicate: Some(Predicate::And(
                Box::new(Predicate::In {
                    field: "a".to_owned(),
                    values: vec![Literal::Str("a".to_owned()), Literal::Str("b".to_owned())],
                }),
                Box::new(Predicate::StartsWith {
                    field: "c".to_owned(),
                    prefix: Literal::Str("n".to_owned()),
                }),
            )),
            ..ast("d")
        }
    );
}

#[test]
fn test_bind_without_predicate_is_noop() {
    assert_eq!(bind(ast("d"), &BTreeMap::new()), Ok(ast("d")));
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

#[rstest]
#[case::conjunction(format!("from d where {}", ["a == 1"; 60].join(" and ")), false)]
#[case::disjunction(format!("from d where {}", ["a == 1"; 60].join(" or ")), false)]
#[case::negation(format!("from d where {}a == 1", "not ".repeat(60)), false)]
#[case::within_limit("from d where a == 1 and b == 2 and c == 3 or not d == 4".to_owned(), true)]
fn test_parse_enforces_boolean_depth(#[case] text: String, #[case] accepted: bool) {
    assert_eq!(parse(&text).is_ok(), accepted);
}

#[rstest]
#[case::empty("")]
#[case::missing_from("select *")]
#[case::missing_domain("from")]
#[case::missing_predicate("from d where")]
#[case::invalid_operator("from d where a =! 1")]
#[case::missing_operator("from d where a b")]
#[case::invalid_escape(r#"from d where a == "bad\q""#)]
#[case::unterminated_string("from d where a == \"open")]
#[case::invalid_timestamp("from d where a == @notatime")]
#[case::missing_parameter("from d where a == :")]
#[case::integer_overflow("from d where a == 999999999999999999999999")]
#[case::unexpected_parenthesis("from d where a == )")]
#[case::non_numeric_limit("from d limit x")]
#[case::negative_limit("from d limit -1")]
#[case::unknown_aggregate("from d aggregate bogus(x) as y by z")]
#[case::malformed_aggregate("from d aggregate count) as y by z")]
#[case::trailing_input("from d extra")]
#[case::membership_without_list("from d where a in 1")]
#[case::invalid_token("from d where # broken")]
fn test_parse_rejects_invalid_query(#[case] text: &str) {
    assert!(matches!(parse(text), Err(PqlError::Parse(_))));
}

#[test]
fn test_parse_limit_accepts_zero_token() {
    assert_eq!(
        parse("from d limit 0"),
        Ok(Ast {
            limit: Some(0),
            ..ast("d")
        })
    );
}

fn ast(domain: &str) -> Ast {
    Ast {
        domain: domain.to_owned(),
        join: None,
        predicate: None,
        selection: Selection::All,
        aggregate: None,
        order_by: Vec::new(),
        limit: None,
    }
}

fn compare(field: &str, op: CompareOp, value: Literal) -> Predicate {
    Predicate::Compare {
        field: field.to_owned(),
        op,
        value,
    }
}

fn params(pairs: &[(&str, Value)]) -> Params {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}
