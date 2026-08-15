//! Direct coverage for the private mappers and the denied-render guard, which the router flow
//! cannot reach: the handler only ever hands render an allowed audience, only classifies the
//! repository and operator field classes it actually emits, and only serializes enums that render
//! as strings.
use peryx_driver::authz::AuthorizationService;
use peryx_identity::{Resource, Scope, UserId};
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use super::{FieldClass, FieldClassification, ResponseAuthorization, StatusCode};
use super::{Page, classification_name, classification_of, render};

#[rstest]
#[case::public(FieldClassification::Public, "public")]
#[case::repository(FieldClassification::Repository, "repository")]
#[case::operator(FieldClassification::Operator, "operator")]
#[case::administrator(FieldClassification::Administrator, "administrator")]
fn test_classification_name(#[case] class: FieldClassification, #[case] expected: &str) {
    assert_eq!(classification_name(class), expected);
}

#[rstest]
#[case::public(FieldClass::Public, FieldClassification::Public)]
#[case::repository(FieldClass::Repository, FieldClassification::Repository)]
#[case::operator(FieldClass::Operator, FieldClassification::Operator)]
#[case::administrator(FieldClass::Administrator, FieldClassification::Administrator)]
fn test_classification_of(#[case] class: FieldClass, #[case] expected: FieldClassification) {
    assert_eq!(classification_of(class), expected);
}

#[test]
fn test_render_denied_audience_is_not_found() {
    // A denied decision cannot classify any field, so render answers 404 rather than an empty body.
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let decision = AuthorizationService::new(meta).authorize_scoped(
        &UserId::random(),
        Scope::AdministrationRead,
        &Resource::Operator,
    );
    assert!(!decision.decision().is_allowed());
    let page = Page {
        outputs: Vec::new(),
        rows: Vec::new(),
        next_cursor: None,
    };
    let response = render(&page, ResponseAuthorization::Scoped(decision));
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
