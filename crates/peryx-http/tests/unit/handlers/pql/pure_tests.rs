//! Direct coverage for the private mappers and the denied-render guard, which the router flow
//! cannot reach: the handler only ever hands render an allowed audience, only classifies the
//! repository and operator field classes it actually emits, and only serializes enums that render
//! as strings.
use peryx_driver::authz::AuthorizationService;
use peryx_identity::{Resource, Scope, UserId};
use peryx_storage::meta::MetaStore;

use super::{FieldClass, FieldClassification, PqlValue, ResponseAuthorization, StatusCode};
use super::{Page, classification_name, classification_of, enum_value, render};

#[test]
fn test_classification_name_covers_every_class() {
    for (class, expected) in [
        (FieldClassification::Public, "public"),
        (FieldClassification::Repository, "repository"),
        (FieldClassification::Operator, "operator"),
        (FieldClassification::Administrator, "administrator"),
    ] {
        assert_eq!(classification_name(class), expected);
    }
}

#[test]
fn test_classification_of_maps_every_field_class() {
    for (class, expected) in [
        (FieldClass::Public, FieldClassification::Public),
        (FieldClass::Repository, FieldClassification::Repository),
        (FieldClass::Operator, FieldClassification::Operator),
        (FieldClass::Administrator, FieldClassification::Administrator),
    ] {
        assert_eq!(classification_of(class), expected);
    }
}

#[test]
fn test_enum_value_falls_back_to_null_for_non_string() {
    // A serializable value that is not a JSON string has no scalar column form, so it becomes null.
    assert_eq!(enum_value(&42_i64), PqlValue::Null);
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
