use std::collections::BTreeSet;

use peryx_core::Ecosystem;
use peryx_driver::route_auth::{AdminRealm, ApiScheme, BASIC_CHALLENGE, ReadExposure, RouteAuth};
use peryx_driver::{Index, IndexKind};
use peryx_identity::IndexAcl;
use peryx_policy::{Policy, PolicyConfig};
use rstest::rstest;
use serde_json::{Value, json};
use utoipa::openapi::ResponseBuilder;

fn index(anonymous_read: bool) -> Index {
    Index {
        name: "source".to_owned(),
        route: "root/source".to_owned(),
        ecosystem: Ecosystem::new("neutral"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::compile(&PolicyConfig::default(), str::to_owned),
        acl: IndexAcl {
            anonymous_read,
            tokens: Vec::new(),
        },
    }
}

fn operation(auth: RouteAuth) -> Value {
    serde_json::to_value(auth.operation(ResponseBuilder::new().description("challenged")).build()).unwrap()
}

#[rstest]
#[case::none(vec![], ReadExposure::Public)]
#[case::open(vec![index(true)], ReadExposure::Public)]
#[case::restricted(vec![index(false)], ReadExposure::Protected)]
#[case::one_restricted(vec![index(true), index(false)], ReadExposure::Protected)]
fn read_exposure_follows_the_configured_index_acls(#[case] indexes: Vec<Index>, #[case] expected: ReadExposure) {
    assert_eq!(ReadExposure::of(&indexes), expected);
}

#[rstest]
#[case::index_access(ApiScheme::IndexAccessToken, "indexAccessToken", "Basic")]
#[case::bearer(ApiScheme::BearerGrant, "bearerGrant", "Bearer")]
#[case::write(ApiScheme::WriteToken, "writeToken", "Basic")]
#[case::administrator(ApiScheme::AdministratorPassword, "administratorPassword", "Basic")]
fn each_scheme_names_the_authorization_it_arrives_in(
    #[case] scheme: ApiScheme,
    #[case] name: &str,
    #[case] auth_scheme: &str,
) {
    assert_eq!((scheme.name(), scheme.auth_scheme()), (name, auth_scheme));
}

#[test]
fn every_declared_scheme_is_an_http_credential_with_a_description() {
    let declarations = ApiScheme::ALL
        .map(|scheme| serde_json::to_value(scheme.declaration()).unwrap())
        .map(|declaration| {
            (
                declaration["type"].as_str().unwrap().to_owned(),
                declaration["scheme"].as_str().unwrap().to_owned(),
                declaration["description"].is_string(),
            )
        });

    assert_eq!(
        declarations.to_vec(),
        vec![
            ("http".to_owned(), "basic".to_owned(), true),
            ("http".to_owned(), "bearer".to_owned(), true),
            ("http".to_owned(), "basic".to_owned(), true),
            ("http".to_owned(), "basic".to_owned(), true),
        ]
    );
}

#[test]
fn the_bearer_scheme_declares_its_token_format() {
    assert_eq!(
        serde_json::to_value(ApiScheme::BearerGrant.declaration()).unwrap()["bearerFormat"],
        json!("JWT")
    );
}

#[rstest]
#[case::public_read(RouteAuth::Read(ReadExposure::Public), vec![])]
#[case::protected_read(
    RouteAuth::Read(ReadExposure::Protected),
    vec![ApiScheme::IndexAccessToken, ApiScheme::BearerGrant],
)]
#[case::write(RouteAuth::Write, vec![ApiScheme::WriteToken, ApiScheme::BearerGrant])]
#[case::administration(RouteAuth::Administration, vec![ApiScheme::AdministratorPassword])]
#[case::write_or_administration(
    RouteAuth::WriteOrAdministration,
    vec![ApiScheme::WriteToken, ApiScheme::AdministratorPassword],
)]
fn each_route_names_the_credentials_it_accepts(#[case] auth: RouteAuth, #[case] expected: Vec<ApiScheme>) {
    assert_eq!(auth.schemes(), expected);
}

/// A read the configuration serves to anyone carries the empty requirement, which is how `OpenAPI`
/// spells anonymous access, and answers no challenge.
#[test]
fn a_public_read_declares_anonymous_access_and_no_challenge() {
    let operation = operation(RouteAuth::Read(ReadExposure::Public));

    assert_eq!(operation["security"], json!([{}]));
    assert_eq!(operation["responses"], json!({}));
}

#[rstest]
#[case::protected_read(
    RouteAuth::Read(ReadExposure::Protected),
    json!([{"indexAccessToken": []}, {"bearerGrant": []}]),
)]
#[case::write(RouteAuth::Write, json!([{"writeToken": []}, {"bearerGrant": []}]))]
#[case::administration(RouteAuth::Administration, json!([{"administratorPassword": []}]))]
#[case::write_or_administration(
    RouteAuth::WriteOrAdministration,
    json!([{"writeToken": []}, {"administratorPassword": []}]),
)]
fn a_guarded_route_lists_its_credentials_and_the_challenge_it_answers(
    #[case] auth: RouteAuth,
    #[case] expected: Value,
) {
    let operation = operation(auth);

    assert_eq!(operation["security"], expected);
    assert_eq!(operation["responses"]["401"]["description"], json!("challenged"));
}

/// Handlers send this value, so a document that describes another one describes a challenge no route
/// answers.
#[test]
fn the_basic_challenge_names_the_peryx_realm() {
    assert_eq!(BASIC_CHALLENGE, "Basic realm=\"peryx\"");
}

/// Every realm is a distinct protection space. Two sharing a challenge would let a credential entered
/// for one surface reach the other, which is why they are enumerated rather than merged.
#[test]
fn every_administration_realm_challenges_in_its_own_protection_space() {
    let challenges: BTreeSet<&str> = AdminRealm::ALL.iter().map(|realm| realm.challenge()).collect();

    assert_eq!(challenges.len(), AdminRealm::ALL.len());
    assert!(
        challenges
            .iter()
            .all(|challenge| challenge.starts_with("Basic realm=\"peryx-")),
        "{challenges:?}"
    );
}

/// The documented `401` repeats the challenge its handler sends, so a reader of the contract knows
/// which protection space a credential belongs to.
#[test]
fn a_realm_documents_the_challenge_its_handler_sends() {
    let documented = serde_json::to_value(AdminRealm::Trash.unauthorized().build()).unwrap();

    assert!(
        documented["description"]
            .as_str()
            .unwrap()
            .contains(AdminRealm::Trash.challenge()),
        "{documented}"
    );
}

/// A route whose credential widens the answer refuses nobody, so it declares what it accepts and
/// documents no challenge.
#[test]
fn a_widening_route_declares_its_credential_and_no_challenge() {
    let operation = serde_json::to_value(RouteAuth::Administration.widening_operation().build()).unwrap();

    assert_eq!(operation["security"], json!([{"administratorPassword": []}]));
    assert_eq!(operation["responses"], json!({}));
}
