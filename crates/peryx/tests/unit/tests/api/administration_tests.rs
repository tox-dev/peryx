use std::collections::{BTreeMap, BTreeSet};

use peryx_driver::route_auth::{AdminRealm, ApiScheme};
use peryx_driver::{RouteDescriptor, RouteMethod, RoutePrincipal};

use crate::api::openapi;

/// The document method key for a descriptor. `GET` also answers `HEAD`, and the document describes
/// the `GET`.
const fn method_key(method: RouteMethod) -> &'static str {
    match method {
        RouteMethod::Delete => "delete",
        RouteMethod::Get => "get",
        RouteMethod::Post => "post",
        RouteMethod::Put => "put",
    }
}

fn document() -> serde_json::Value {
    serde_json::to_value(openapi()).unwrap()
}

/// Whether the operation at `(path, method)` requires a local administrator.
fn requires_administrator(document: &serde_json::Value, path: &str, method: &str) -> bool {
    document["paths"][path][method]["security"]
        .as_array()
        .is_some_and(|requirements| {
            requirements
                .iter()
                .any(|requirement| requirement.get(ApiScheme::AdministratorPassword.name()).is_some())
        })
}

/// The routes the process router serves, keyed as the document keys them.
fn administration_routes() -> BTreeMap<(String, &'static str), bool> {
    peryx_http::service_route_descriptors()
        .into_iter()
        .map(|descriptor: RouteDescriptor| {
            (
                (descriptor.path().to_owned(), method_key(descriptor.method())),
                descriptor.principal() == RoutePrincipal::LocalUser,
            )
        })
        .collect()
}

/// The router already says which routes check a server user's password, so the declaration is read
/// against that rather than against a list kept beside it. A management route added without its
/// credential, or a credential declared on a route that never checks one, fails here.
#[test]
fn test_every_route_that_checks_a_local_user_declares_the_administrator_credential() {
    let document = document();

    let mismatched: Vec<_> = administration_routes()
        .into_iter()
        .filter_map(|((path, method), local_user)| {
            let declared = requires_administrator(&document, &path, method);
            (declared != local_user).then_some((path, method, local_user))
        })
        .collect();

    assert_eq!(mismatched, Vec::new());
}

/// Every administration operation describes the `401` its handler sends, and the realm it names is one
/// the model owns. A handler that changes protection space without the document following fails here.
#[test]
fn test_a_guarded_administration_operation_names_a_realm_the_model_owns() {
    let document = document();
    let realms: BTreeSet<&str> = AdminRealm::ALL.iter().map(|realm| realm.challenge()).collect();

    let unnamed: Vec<_> = administration_routes()
        .into_iter()
        .filter(|(_, local_user)| *local_user)
        .filter_map(|((path, method), _)| {
            let description = document["paths"][&path][method]["responses"]["401"]["description"].as_str();
            description.map(|description| (path, method, realms.iter().any(|realm| description.contains(realm))))
        })
        .filter(|(.., named)| !named)
        .collect();

    assert_eq!(unnamed, Vec::new());
}

/// A route that widens its answer for an administrator rather than refusing without one documents no
/// challenge, because it turns nobody away. Pinning the two that behave this way keeps a later route
/// from quietly joining them.
#[test]
fn test_the_routes_that_widen_rather_than_challenge_document_no_401() {
    let document = document();

    let widening: Vec<String> = administration_routes()
        .into_iter()
        .filter(|(_, local_user)| *local_user)
        .filter(|((path, method), _)| document["paths"][path][method]["responses"]["401"].is_null())
        .map(|((path, _), _)| path)
        .collect();

    assert_eq!(widening, ["/+status"]);
}
