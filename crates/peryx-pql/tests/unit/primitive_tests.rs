use crate::ast::{AggregateFunc, CompareOp};
use crate::catalog::{Column, FieldClass, Indexability};
use crate::error::{PqlError, StatusClass};
use crate::scope::{QueryScope, RepoScope};
use crate::value::ValueType;
use rstest::rstest;

use super::support::schema;

#[rstest]
#[case::equal(CompareOp::Eq, "==")]
#[case::not_equal(CompareOp::Ne, "!=")]
#[case::less_than(CompareOp::Lt, "<")]
#[case::less_or_equal(CompareOp::Le, "<=")]
#[case::greater_than(CompareOp::Gt, ">")]
#[case::greater_or_equal(CompareOp::Ge, ">=")]
fn test_compare_op_spelling(#[case] op: CompareOp, #[case] expected: &str) {
    assert_eq!(op.as_str(), expected);
}

#[rstest]
#[case::sum(AggregateFunc::Sum, "sum", true)]
#[case::count(AggregateFunc::Count, "count", false)]
#[case::minimum(AggregateFunc::Min, "min", true)]
#[case::maximum(AggregateFunc::Max, "max", true)]
fn test_aggregate_func_spelling_and_arity(
    #[case] func: AggregateFunc,
    #[case] expected: &str,
    #[case] needs_column: bool,
) {
    assert_eq!((func.as_str(), func.needs_column()), (expected, needs_column));
}

#[test]
fn test_field_class_ordering_and_most_restrictive() {
    assert!(FieldClass::Public < FieldClass::Repository);
    assert!(FieldClass::Repository < FieldClass::Operator);
    assert!(FieldClass::Operator < FieldClass::Administrator);
    assert_eq!(
        FieldClass::Repository.most_restrictive(FieldClass::Operator),
        FieldClass::Operator
    );
    assert_eq!(
        FieldClass::Administrator.most_restrictive(FieldClass::Public),
        FieldClass::Administrator
    );
}

#[test]
fn test_indexability_cheapness() {
    assert!(Indexability::KeyOrdered.is_cheap());
    assert!(Indexability::Indexed.is_cheap());
    assert!(!Indexability::Scan.is_cheap());
}

#[test]
fn test_column_constructor_fields() {
    let column = Column::new("c", ValueType::Int, FieldClass::Operator, Indexability::Indexed, true);
    assert_eq!(column.name, "c");
    assert_eq!(column.value_type, ValueType::Int);
    assert_eq!(column.class, FieldClass::Operator);
    assert_eq!(column.indexability, Indexability::Indexed);
    assert!(column.numeric);
}

#[test]
fn test_domain_schema_column_lookup() {
    let schema = schema();
    assert!(schema.column("repository").is_some());
    assert!(schema.column("nope").is_none());
}

#[rstest]
#[case::parse(PqlError::Parse(String::new()), StatusClass::BadRequest)]
#[case::validation(PqlError::Validation(String::new()), StatusClass::BadRequest)]
#[case::parameter(PqlError::MissingParameter(String::new()), StatusClass::BadRequest)]
#[case::cost(PqlError::CostExceeded(String::new()), StatusClass::BadRequest)]
#[case::join(PqlError::UnboundedJoin(String::new()), StatusClass::BadRequest)]
#[case::cursor(PqlError::InvalidCursor, StatusClass::BadRequest)]
#[case::scope(PqlError::CursorScopeChanged, StatusClass::BadRequest)]
#[case::unauthorized(PqlError::Unauthorized, StatusClass::NotFound)]
#[case::backend(PqlError::Backend(String::new()), StatusClass::Unavailable)]
fn test_error_status_class(#[case] error: PqlError, #[case] expected: StatusClass) {
    assert_eq!(error.status(), expected);
}

#[test]
fn test_error_display_hides_detail() {
    assert_eq!(
        PqlError::Parse("secret token".to_owned()).to_string(),
        "could not parse the query"
    );
    assert_eq!(
        PqlError::CursorScopeChanged.to_string(),
        "the caller's scope changed; restart the query"
    );
}

#[test]
fn test_repo_scope_permits() {
    use std::collections::BTreeSet;
    assert!(RepoScope::All.permits("anything"));
    let scope = QueryScope::new(
        RepoScope::Only(BTreeSet::from(["a".to_owned()])),
        super::support::repository_classes(),
        "fp".to_owned(),
    );
    assert!(scope.repositories().permits("a"));
    assert!(!scope.repositories().permits("b"));
    assert_eq!(scope.fingerprint(), "fp");
    assert_eq!(*scope.repositories(), RepoScope::Only(BTreeSet::from(["a".to_owned()])));
}
