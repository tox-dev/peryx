use rstest::rstest;

use crate::catalog::{FieldClass, FieldVisibility};
use crate::error::PqlError;
use crate::execute::{Page, execute};
use crate::parse::parse;
use crate::plan::OutputColumn;
use crate::value::{Value, ValueType};

use super::support::{TestSource, repository_classes, repository_scope, scoped, usage_schema};

#[rstest]
#[case::comparison("from usage where bytes >= 5 select resource")]
#[case::membership("from usage where bytes in (5, 10) select resource")]
#[case::negation("from usage where not bytes == 5 select resource")]
#[case::selection("from usage select resource, bytes")]
#[case::group_key("from usage aggregate count() as n by bytes")]
#[case::aggregate_input("from usage aggregate sum(bytes) as total by resource")]
#[case::join_projection("from policy.decisions join usage on repository, resource select resource, bytes")]
fn test_execute_rejects_a_column_above_the_caller_class_before_fetch(#[case] text: &str) {
    let source = TestSource::new(Vec::new());
    assert_eq!(
        (
            execute(&parse(text).expect("parses"), &repository_scope("alpha"), None, &source),
            source.fetches(),
        ),
        (
            Err(PqlError::Validation("unknown column `bytes`".to_owned())),
            Vec::new(),
        )
    );
}

#[test]
fn test_execute_rejects_ordering_by_a_column_above_the_caller_class() {
    let source = TestSource::new(Vec::new());
    assert_eq!(
        (
            execute(
                &parse("from usage order by bytes desc").expect("parses"),
                &repository_scope("alpha"),
                None,
                &source,
            ),
            source.fetches(),
        ),
        (
            Err(PqlError::Validation(
                "cannot order by `bytes`; it is not a selected column".to_owned()
            )),
            Vec::new(),
        )
    );
}

#[test]
fn test_execute_rejects_a_join_key_above_the_caller_class() {
    // `big.name` is public, so the outer domain stays readable to a caller without repository
    // fields while the `repository` key both domains share does not.
    let source = TestSource::new(Vec::new());
    let scope = scoped(
        "alpha",
        FieldVisibility::new([FieldClass::Public, FieldClass::Operator]),
    );
    assert_eq!(
        (
            execute(
                &parse("from big join policy.decisions on repository").expect("parses"),
                &scope,
                None,
                &source,
            ),
            source.fetches(),
        ),
        (
            Err(PqlError::Validation("unknown column `repository`".to_owned())),
            Vec::new(),
        )
    );
}

#[test]
fn test_execute_hides_a_domain_whose_natural_order_is_above_the_caller_class() {
    // `notes` orders on an operator column, so a repository caller cannot page it at all and the
    // domain answers as one they cannot read.
    let source = TestSource::new(Vec::new());
    assert_eq!(
        (
            execute(
                &parse("from notes").expect("parses"),
                &repository_scope("alpha"),
                None,
                &source,
            ),
            source.fetches(),
        ),
        (Err(PqlError::Unauthorized), Vec::new())
    );
}

#[test]
fn test_execute_wildcard_projects_only_visible_columns() {
    assert_eq!(
        execute(
            &parse("from usage").expect("parses"),
            &repository_scope("alpha"),
            None,
            &TestSource::new(Vec::new()),
        ),
        Ok(Page {
            outputs: vec![
                output("repository", ValueType::Str),
                output("resource", ValueType::Str),
                output("hits", ValueType::Int),
            ],
            rows: vec![
                vec![
                    Value::Str("alpha".to_owned()),
                    Value::Str("resource-a".to_owned()),
                    Value::Int(100),
                ],
                vec![
                    Value::Str("alpha".to_owned()),
                    Value::Str("resource-b".to_owned()),
                    Value::Int(50),
                ],
            ],
            next_cursor: None,
        })
    );
}

#[test]
fn test_execute_keeps_operator_columns_for_an_operator_caller() {
    assert_eq!(
        execute(
            &parse("from usage where bytes >= 5 select resource, bytes").expect("parses"),
            &super::support::operator_scope(),
            None,
            &TestSource::new(Vec::new()),
        )
        .expect("runs")
        .rows,
        vec![
            vec![Value::Str("resource-a".to_owned()), Value::Int(10)],
            vec![Value::Str("resource-b".to_owned()), Value::Int(5)],
        ]
    );
}

#[rstest]
#[case::public(FieldClass::Public, true)]
#[case::repository(FieldClass::Repository, true)]
#[case::operator(FieldClass::Operator, false)]
#[case::administrator(FieldClass::Administrator, false)]
fn test_field_visibility_permits_only_the_classes_it_holds(#[case] class: FieldClass, #[case] permitted: bool) {
    assert_eq!(repository_classes().permits(class), permitted);
}

#[test]
fn test_visible_to_keeps_the_rest_of_the_schema() {
    let visible = usage_schema().visible_to(&repository_classes());
    assert_eq!(
        (
            visible.name,
            visible.natural_order,
            visible.bounded,
            visible.pushdown,
            visible.columns.iter().map(|column| column.name).collect::<Vec<_>>(),
        ),
        (
            "usage",
            "hits",
            true,
            ["repository", "resource"].as_slice(),
            vec!["repository", "resource", "hits"],
        )
    );
}

fn output(name: &str, value_type: ValueType) -> OutputColumn {
    OutputColumn {
        name: name.to_owned(),
        class: FieldClass::Repository,
        value_type,
    }
}
