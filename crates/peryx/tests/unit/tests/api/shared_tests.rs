use std::collections::BTreeSet;

use utoipa::openapi::PathsBuilder;

use crate::api::{openapi, openapi_for, openapi_json, openapi_json_for};

// Sorted entries reduce merge conflicts between endpoint additions.
#[test]
fn test_openapi_document_covers_every_endpoint() {
    let spec = serde_json::to_value(openapi()).unwrap();
    let documented: BTreeSet<String> = spec["paths"].as_object().unwrap().keys().cloned().collect();
    let plugin_spec =
        serde_json::to_value(crate::compiled_plugins().openapi_paths(PathsBuilder::new()).build()).unwrap();
    let plugin_paths: BTreeSet<String> = plugin_spec.as_object().unwrap().keys().cloned().collect();
    let core_paths: BTreeSet<String> = documented.difference(&plugin_paths).cloned().collect();
    let expected = BTreeSet::from(
        [
            "/+acl",
            "/+analytics/completeness",
            "/+analytics/sources",
            "/+analytics/timeline",
            "/+analytics/top-resources",
            "/+analytics/unused",
            "/+analytics/groups",
            "/+api",
            "/+availability/operations",
            "/+availability/placements",
            "/+availability/placements/{digest}",
            "/+availability/topology",
            "/+availability/topology/stream",
            "/+grants",
            "/+grants/{id}",
            "/+health",
            "/+jobs/{id}/cancel",
            "/+policy/decisions",
            "/+query",
            "/+quota",
            "/+quota/repository",
            "/+ready",
            "/+repositories",
            "/+repositories/{id}",
            "/+repositories/{id}/disable",
            "/+repositories/{id}/enable",
            "/+retention/export",
            "/+retention/plan",
            "/+revocations",
            "/+revocations/{digest}",
            "/+revocations/{digest}/lift",
            "/+search",
            "/+shadow/candidates",
            "/+stats",
            "/+status",
            "/+tokens",
            "/+tokens/{id}",
            "/+tokens/{id}/rotate",
            "/+trash",
            "/+trash/record",
            "/api-docs/openapi.json",
            "/metrics",
        ]
        .map(str::to_owned),
    );
    assert_eq!(core_paths, expected);
    assert_eq!(spec["info"]["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn test_openapi_json_has_stable_object_order() {
    assert_json_objects_are_sorted(&serde_json::from_str(&openapi_json()).unwrap());
}

#[test]
fn test_none_openapi_omits_distributed_routes() {
    let spec = serde_json::to_value(openapi_for(peryx_ha::AvailabilityResources::None)).unwrap();
    let paths = spec["paths"].as_object().unwrap();

    assert!(!paths.contains_key("/+analytics/completeness"));
    assert!(!paths.keys().any(|path| path.starts_with("/+availability/")));
}

#[test]
fn test_none_openapi_json_matches_the_none_document() {
    let json = openapi_json_for(peryx_ha::AvailabilityResources::None);

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap(),
        serde_json::to_value(openapi_for(peryx_ha::AvailabilityResources::None)).unwrap()
    );
    assert!(json.ends_with('\n'));
}

fn assert_json_objects_are_sorted(value: &serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => values.iter().for_each(assert_json_objects_are_sorted),
        serde_json::Value::Object(object) => {
            assert!(object.keys().is_sorted(), "object keys are not sorted: {object:?}");
            object.values().for_each(assert_json_objects_are_sorted);
        }
        _ => {}
    }
}
