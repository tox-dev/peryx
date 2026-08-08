use crate::ast::{AggregateFunc, CompareOp};
use crate::catalog::{Column, FieldClass, Indexability};
use crate::error::{PqlError, StatusClass};
use crate::scope::{QueryScope, RepoScope};
use crate::value::ValueType;

use super::support::schema;

#[test]
fn test_compare_op_spellings() {
    for (op, text) in [
        (CompareOp::Eq, "=="),
        (CompareOp::Ne, "!="),
        (CompareOp::Lt, "<"),
        (CompareOp::Le, "<="),
        (CompareOp::Gt, ">"),
        (CompareOp::Ge, ">="),
    ] {
        assert_eq!(op.as_str(), text);
    }
}

#[test]
fn test_aggregate_func_spelling_and_arity() {
    assert_eq!(AggregateFunc::Sum.as_str(), "sum");
    assert_eq!(AggregateFunc::Count.as_str(), "count");
    assert_eq!(AggregateFunc::Min.as_str(), "min");
    assert_eq!(AggregateFunc::Max.as_str(), "max");
    assert!(AggregateFunc::Sum.needs_column());
    assert!(AggregateFunc::Min.needs_column());
    assert!(AggregateFunc::Max.needs_column());
    assert!(!AggregateFunc::Count.needs_column());
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

#[test]
fn test_error_status_classes() {
    assert_eq!(PqlError::Parse(String::new()).status(), StatusClass::BadRequest);
    assert_eq!(PqlError::Validation(String::new()).status(), StatusClass::BadRequest);
    assert_eq!(
        PqlError::MissingParameter(String::new()).status(),
        StatusClass::BadRequest
    );
    assert_eq!(PqlError::CostExceeded(String::new()).status(), StatusClass::BadRequest);
    assert_eq!(PqlError::UnboundedJoin(String::new()).status(), StatusClass::BadRequest);
    assert_eq!(PqlError::InvalidCursor.status(), StatusClass::BadRequest);
    assert_eq!(PqlError::CursorScopeChanged.status(), StatusClass::BadRequest);
    assert_eq!(PqlError::Unauthorized.status(), StatusClass::NotFound);
    assert_eq!(PqlError::Backend(String::new()).status(), StatusClass::Unavailable);
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
    let scope = QueryScope::new(RepoScope::Only(BTreeSet::from(["a".to_owned()])), "fp".to_owned());
    assert!(scope.repositories().permits("a"));
    assert!(!scope.repositories().permits("b"));
    assert_eq!(scope.fingerprint(), "fp");
    assert_eq!(*scope.repositories(), RepoScope::Only(BTreeSet::from(["a".to_owned()])));
}
